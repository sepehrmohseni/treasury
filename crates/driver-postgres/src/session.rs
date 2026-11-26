//! One PostgreSQL connection.

use crate::binary::{Decoded, Param};
use crate::error;
use crate::text;
use crate::tls::{MakeRustls, RustlsStream};
use async_trait::async_trait;
use futures::{Stream, StreamExt, stream};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio_postgres::types::Type;
use tokio_postgres::{AsyncMessage, CancelToken, Client, Connection, SimpleQueryMessage, Socket};
use treasury_core::analyze::Analyzer;
use treasury_core::driver::{
    Cancel, CancelMethod, Capabilities, Edit, Execute, Explain, ExplainSupport, Health, Introspect,
    QueryLanguage, ReadOnlyEnforcement, ServerInfo, Session, SqlDialect,
};
use treasury_core::exec::{
    Column, ExecEvent, ExecRequest, ExecStream, Notice, NoticeLevel, Row, ValueKind,
};
use treasury_core::{Error, ErrorKind, Result, Value};
use treasury_sql::SqlAnalyzer;

type Notices = Arc<Mutex<VecDeque<Notice>>>;

/// An open PostgreSQL session.
pub struct PgSession {
    pub(crate) client: Arc<Client>,
    cancel_token: CancelToken,
    tls: MakeRustls,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) metadata_timeout: Duration,
    notices: Notices,
    closed: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    analyzer: SqlAnalyzer,
}

impl PgSession {
    pub(crate) fn start(
        client: Client,
        mut connection: Connection<Socket, RustlsStream<Socket>>,
        tls: MakeRustls,
        host: String,
        port: u16,
        metadata_timeout: Duration,
    ) -> Self {
        let notices: Notices = Arc::new(Mutex::new(VecDeque::new()));
        let closed = Arc::new(AtomicBool::new(false));
        {
            let notices = notices.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                // Drives the connection and collects what the server says between results.
                let mut messages = stream::poll_fn(move |cx| connection.poll_message(cx));
                while let Some(m) = messages.next().await {
                    let notice = match m {
                        Ok(AsyncMessage::Notice(n)) => Notice {
                            level: if n.severity() == "WARNING" {
                                NoticeLevel::Warning
                            } else {
                                NoticeLevel::Info
                            },
                            message: n.message().to_owned(),
                            code: Some(n.code().code().to_owned()),
                        },
                        Ok(AsyncMessage::Notification(n)) => Notice {
                            level: NoticeLevel::Info,
                            message: format!("NOTIFY {}: {}", n.channel(), n.payload()),
                            code: None,
                        },
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::debug!("postgres connection ended: {e}");
                            break;
                        }
                    };
                    let mut q = notices.lock().unwrap_or_else(PoisonError::into_inner);
                    if q.len() < 1_000 {
                        q.push_back(notice);
                    }
                }
                closed.store(true, Ordering::SeqCst);
            });
        }
        Self {
            cancel_token: client.cancel_token(),
            client: Arc::new(client),
            tls,
            host,
            port,
            metadata_timeout,
            notices,
            closed,
            timed_out: Arc::new(AtomicBool::new(false)),
            analyzer: SqlAnalyzer::new(SqlDialect::Postgres),
        }
    }

    pub(crate) fn map(&self, e: &tokio_postgres::Error) -> Error {
        error::map(e, &self.host, self.port)
    }

    pub(crate) fn alive(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) || self.client.is_closed() {
            return Err(Error::network(
                &self.host,
                self.port,
                &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
            ));
        }
        Ok(())
    }

    /// Runs `work` under the metadata time limit, cancelling the statement on the server when
    /// the limit passes.
    pub(crate) async fn timed<T>(
        &self,
        work: impl std::future::Future<Output = std::result::Result<T, tokio_postgres::Error>>,
    ) -> Result<T> {
        self.alive()?;
        match tokio::time::timeout(self.metadata_timeout, work).await {
            Ok(r) => r.map_err(|e| self.map(&e)),
            Err(_) => {
                let _ = self.cancel_token.cancel_query(self.tls.clone()).await;
                Err(Error::timeout(self.metadata_timeout))
            }
        }
    }

    /// Runs a metadata query under the metadata time limit.
    pub(crate) async fn meta(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>> {
        self.timed(self.client.query(sql, params)).await
    }

    /// Asks the server to stop the statement running on this session.
    pub(crate) async fn cancel_query(&self) -> std::result::Result<(), tokio_postgres::Error> {
        self.cancel_token.cancel_query(self.tls.clone()).await
    }

    /// Column `i` of `row`.
    pub(crate) fn col<'a, T: tokio_postgres::types::FromSql<'a>>(
        &self,
        row: &'a tokio_postgres::Row,
        i: usize,
    ) -> Result<T> {
        row.try_get(i).map_err(|e| self.map(&e))
    }

    /// The first row of a metadata query, if any.
    pub(crate) async fn meta_one(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Option<tokio_postgres::Row>> {
        Ok(self.meta(sql, params).await?.into_iter().next())
    }

    fn take_notices(notices: &Notices) -> Vec<Notice> {
        notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}

pub(crate) fn kind_of(ty: &Type) -> ValueKind {
    use tokio_postgres::types::Kind;
    match ty.kind() {
        Kind::Array(_) => return ValueKind::List,
        Kind::Composite(_) => return ValueKind::Composite,
        Kind::Domain(base) => return kind_of(base),
        Kind::Enum(_) => return ValueKind::Text,
        _ => {}
    }
    match *ty {
        Type::BOOL => ValueKind::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => ValueKind::Integer,
        Type::FLOAT4 | Type::FLOAT8 => ValueKind::Float,
        Type::NUMERIC | Type::MONEY => ValueKind::Decimal,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::CHAR | Type::XML => {
            ValueKind::Text
        }
        Type::BYTEA => ValueKind::Binary,
        Type::DATE => ValueKind::Date,
        Type::TIME | Type::TIMETZ => ValueKind::Time,
        Type::TIMESTAMP => ValueKind::Timestamp,
        Type::TIMESTAMPTZ => ValueKind::TimestampTz,
        Type::INTERVAL => ValueKind::Interval,
        Type::UUID => ValueKind::Uuid,
        Type::JSON | Type::JSONB => ValueKind::Json,
        _ => ValueKind::Other,
    }
}

fn column(c: &tokio_postgres::Column) -> Column {
    Column::new(c.name(), c.type_().name(), kind_of(c.type_()))
}

/// Wraps an event stream: interleaves server notices and turns a cancellation the session's own
/// timer caused into a timeout.
struct Events {
    inner: ExecStream,
    notices: Notices,
    pending: VecDeque<Result<ExecEvent>>,
    timed_out: Arc<AtomicBool>,
    limit: Option<Duration>,
    done: bool,
}

impl Stream for Events {
    type Item = Result<ExecEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if let Some(e) = this.pending.pop_front() {
            return Poll::Ready(Some(e));
        }
        if this.done {
            let rest = PgSession::take_notices(&this.notices);
            if rest.is_empty() {
                return Poll::Ready(None);
            }
            this.pending
                .extend(rest.into_iter().map(|n| Ok(ExecEvent::Notice(n))));
            return Poll::Ready(this.pending.pop_front());
        }
        match this.inner.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.done = true;
                self.poll_next(cx)
            }
            Poll::Ready(Some(item)) => {
                let item = match item {
                    Err(e)
                        if e.kind == ErrorKind::Cancelled
                            && this.timed_out.swap(false, Ordering::SeqCst) =>
                    {
                        Err(Error::timeout(this.limit.unwrap_or_default()))
                    }
                    other => other,
                };
                let notices = PgSession::take_notices(&this.notices);
                this.pending
                    .extend(notices.into_iter().map(|n| Ok(ExecEvent::Notice(n))));
                this.pending.push_back(item);
                Poll::Ready(this.pending.pop_front())
            }
        }
    }
}

#[async_trait]
impl Session for PgSession {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            language: QueryLanguage::Sql(SqlDialect::Postgres),
            introspect: true,
            ddl: true,
            explain: Some(ExplainSupport { analyze: true }),
            cancel: CancelMethod::Native,
            health: true,
            edit: true,
            transactions: true,
            read_only: ReadOnlyEnforcement::Server,
            multiple_results: false,
        }
    }

    fn analyzer(&self) -> &dyn Analyzer {
        &self.analyzer
    }

    async fn ping(&self) -> Result<Duration> {
        self.alive()?;
        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            self.client.simple_query("SELECT 1"),
        )
        .await
        .map_err(|_| Error::timeout(Duration::from_secs(10)))?
        .map_err(|e| self.map(&e))?;
        Ok(started.elapsed())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let rows = self
            .meta("SELECT current_setting('server_version'), current_user::text, current_database()::text, \
                   pg_is_in_recovery()", &[])
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::protocol("no server facts"))?;
        let version: String = row.try_get(0).map_err(|e| self.map(&e))?;
        let user: String = row.try_get(1).map_err(|e| self.map(&e))?;
        let db: String = row.try_get(2).map_err(|e| self.map(&e))?;
        let standby: bool = row.try_get(3).map_err(|e| self.map(&e))?;
        Ok(ServerInfo {
            product: "PostgreSQL".into(),
            version,
            details: vec![
                ("User".into(), user),
                ("Database".into(), db),
                (
                    "Role".into(),
                    if standby {
                        "standby".into()
                    } else {
                        "primary".into()
                    },
                ),
            ],
        })
    }

    async fn set_read_only(&self, read_only: bool) -> Result<()> {
        self.alive()?;
        let sql = if read_only {
            "SET default_transaction_read_only = on"
        } else {
            "SET default_transaction_read_only = off"
        };
        self.client
            .batch_execute(sql)
            .await
            .map_err(|e| self.map(&e))
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the last client handle ends the connection task; ask the server to stop any
        // running statement first so it does not keep working for nobody.
        let _ = self.cancel_token.cancel_query(self.tls.clone()).await;
        Ok(())
    }

    fn execute(&self) -> Option<&dyn Execute> {
        Some(self)
    }

    fn cancel(&self) -> Option<&dyn Cancel> {
        Some(self)
    }

    fn introspect(&self) -> Option<&dyn Introspect> {
        Some(self)
    }

    fn explain(&self) -> Option<&dyn Explain> {
        Some(self)
    }

    fn health(&self) -> Option<&dyn Health> {
        Some(self)
    }

    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
}

#[async_trait]
impl Execute for PgSession {
    async fn execute(&self, req: ExecRequest) -> Result<ExecStream> {
        self.alive()?;
        let head = req
            .text
            .trim_start()
            .get(..4)
            .unwrap_or("")
            .to_ascii_uppercase();
        if head == "COPY" {
            return Err(Error::unsupported(
                "COPY needs a file on the client side. Use Export or Import for files.",
            ));
        }
        // Preparing gives the column types and proves the text is one statement: the guardrails
        // decided about one statement, so several are never sent together.
        let prepared = match self.client.prepare(&req.text).await {
            Ok(s) => s,
            Err(e)
                if e.as_db_error()
                    .is_some_and(|d| d.message().contains("multiple commands")) =>
            {
                return Err(Error::unsupported(
                    "This text holds more than one statement. Treasury runs them one at a time.",
                ));
            }
            Err(e) => return Err(self.map(&e)),
        };
        if let Some(limit) = req.timeout {
            let token = self.cancel_token.clone();
            let tls = self.tls.clone();
            let flag = self.timed_out.clone();
            self.timed_out.store(false, Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                let _ = token.cancel_query(tls).await;
            });
        }
        let inner: ExecStream = if req.params.is_empty() {
            self.simple(&req, &prepared).await?
        } else {
            self.extended(&req, prepared).await?
        };
        Ok(Box::pin(Events {
            inner,
            notices: self.notices.clone(),
            pending: VecDeque::new(),
            timed_out: self.timed_out.clone(),
            limit: req.timeout,
            done: false,
        }))
    }
}

impl PgSession {
    async fn simple(
        &self,
        req: &ExecRequest,
        prepared: &tokio_postgres::Statement,
    ) -> Result<ExecStream> {
        let types: Arc<Vec<Type>> = Arc::new(
            prepared
                .columns()
                .iter()
                .map(|c| c.type_().clone())
                .collect(),
        );
        let columns: Vec<Column> = prepared.columns().iter().map(column).collect();
        let raw = self
            .client
            .simple_query_raw(&req.text)
            .await
            .map_err(|e| self.map(&e))?;
        let host = self.host.clone();
        let port = self.port;
        let batch = usize::try_from(req.fetch_rows.max(1)).unwrap_or(500);
        let events = raw
            .ready_chunks(batch)
            .flat_map(move |chunk| {
                let mut out: Vec<Result<ExecEvent>> = Vec::new();
                let mut rows: Vec<Row> = Vec::new();
                for msg in chunk {
                    match msg {
                        Ok(SimpleQueryMessage::RowDescription(desc)) => {
                            let cols = if columns.len() == desc.len() {
                                columns.clone()
                            } else {
                                desc.iter()
                                    .map(|c| Column::new(c.name(), "text", ValueKind::Text))
                                    .collect()
                            };
                            out.push(Ok(ExecEvent::ResultStart(cols)));
                        }
                        Ok(SimpleQueryMessage::Row(r)) => {
                            let row: Row = (0..r.len())
                                .map(|i| match r.get(i) {
                                    None => Value::Null,
                                    Some(t) => match types.get(i) {
                                        Some(ty) => text::parse(ty, t),
                                        None => Value::Text(t.to_owned()),
                                    },
                                })
                                .collect();
                            rows.push(row);
                        }
                        Ok(SimpleQueryMessage::CommandComplete(n)) => {
                            if !rows.is_empty() {
                                out.push(Ok(ExecEvent::Rows(std::mem::take(&mut rows))));
                            }
                            let returned_rows = out
                                .iter()
                                .any(|e| matches!(e, Ok(ExecEvent::ResultStart(_))));
                            out.push(Ok(ExecEvent::ResultEnd {
                                affected: if returned_rows { None } else { Some(n) },
                                truncated: false,
                            }));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if !rows.is_empty() {
                                out.push(Ok(ExecEvent::Rows(std::mem::take(&mut rows))));
                            }
                            out.push(Err(error::map(&e, &host, port)));
                        }
                    }
                }
                if !rows.is_empty() {
                    out.push(Ok(ExecEvent::Rows(rows)));
                }
                stream::iter(out)
            })
            .boxed();
        Ok(events)
    }

    async fn extended(
        &self,
        req: &ExecRequest,
        stmt: tokio_postgres::Statement,
    ) -> Result<ExecStream> {
        let params: Vec<Param> = req.params.iter().map(|p| Param(p.value.clone())).collect();
        let columns: Vec<Column> = stmt.columns().iter().map(column).collect();
        if columns.is_empty() {
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
                .iter()
                .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
                .collect();
            let n = self
                .client
                .execute(&stmt, &refs)
                .await
                .map_err(|e| self.map(&e))?;
            return Ok(stream::iter([Ok(ExecEvent::ResultEnd {
                affected: Some(n),
                truncated: false,
            })])
            .boxed());
        }
        let rows = self
            .client
            .query_raw(&stmt, params)
            .await
            .map_err(|e| self.map(&e))?;
        let host = self.host.clone();
        let port = self.port;
        let width = columns.len();
        let batch = usize::try_from(req.fetch_rows.max(1)).unwrap_or(500);
        let body = rows.ready_chunks(batch).map(move |chunk| {
            let mut out = Vec::with_capacity(chunk.len());
            for r in chunk {
                let r = r.map_err(|e| error::map(&e, &host, port))?;
                let mut row = Vec::with_capacity(width);
                for i in 0..width {
                    let Decoded(v) = r.try_get(i).map_err(|e| error::map(&e, &host, port))?;
                    row.push(v);
                }
                out.push(row);
            }
            Ok(ExecEvent::Rows(out))
        });
        Ok(stream::iter([Ok(ExecEvent::ResultStart(columns))])
            .chain(body)
            .chain(stream::iter([Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            })]))
            .boxed())
    }
}

#[async_trait]
impl Cancel for PgSession {
    async fn cancel(&self) -> Result<()> {
        self.cancel_token
            .cancel_query(self.tls.clone())
            .await
            .map_err(|e| self.map(&e))
    }
}
