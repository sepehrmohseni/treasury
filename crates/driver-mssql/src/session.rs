//! One SQL Server connection.

use crate::error;
use crate::value::{self, Param};
use async_trait::async_trait;
use futures::{TryStreamExt, stream};
use secrecy::ExposeSecret;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, QueryItem, ToSql};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use treasury_core::analyze::Analyzer;
use treasury_core::config::{ConnectionConfig, Endpoint, SecretSlot, TlsMode};
use treasury_core::driver::{
    Cancel, CancelMethod, Capabilities, ConnectContext, Edit, Execute, Explain, ExplainSupport,
    Health, Introspect, QueryLanguage, ReadOnlyEnforcement, ServerInfo, Session, SqlDialect,
};
use treasury_core::exec::{ExecEvent, ExecRequest, ExecStream};
use treasury_core::{Error, Result, Value};
use treasury_sql::SqlAnalyzer;

pub(crate) type Conn = Client<Compat<TcpStream>>;

const ONE_AT_A_TIME: &str =
    "This text holds more than one batch. Treasury runs them one at a time.";

/// An open SQL Server session.
pub struct MsSqlSession {
    pub(crate) client: Arc<Mutex<Option<Conn>>>,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) metadata_timeout: Duration,
    analyzer: SqlAnalyzer,
    closed: AtomicBool,
    /// Signals the statement currently running, if any.
    current: std::sync::Mutex<Option<Arc<Notify>>>,
    timed_out: Arc<AtomicBool>,
}

async fn dial(host: &str, port: u16, limit: Duration) -> Result<TcpStream> {
    let tcp = tokio::time::timeout(limit, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::timeout(limit))?
        .map_err(|e| Error::network(host, port, &e))?;
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

pub(crate) async fn connect(
    config: &ConnectionConfig,
    ctx: ConnectContext,
) -> Result<MsSqlSession> {
    // tiberius takes rustls' process-wide provider; Treasury only ever installs ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Endpoint::Host { host, port } = &config.endpoint else {
        return Err(Error::config(
            Some("host"),
            "SQL Server connections need a host and port.",
        ));
    };
    let password = ctx.secrets.secret(SecretSlot::Password)?;
    let mut cfg = Config::new();
    cfg.host(host);
    cfg.port(*port);
    cfg.application_name("Treasury");
    if let Some(db) = &config.database {
        cfg.database(db);
    }
    cfg.authentication(AuthMethod::sql_server(
        config.user.clone().unwrap_or_else(|| "sa".to_owned()),
        password
            .as_ref()
            .map(|p| p.expose_secret())
            .unwrap_or_default(),
    ));
    cfg.encryption(match config.tls.mode {
        TlsMode::Disable => EncryptionLevel::NotSupported,
        TlsMode::Prefer => EncryptionLevel::On,
        TlsMode::Require => EncryptionLevel::Required,
    });
    if config.tls.accept_invalid {
        cfg.trust_cert();
    } else if let Some(ca) = &config.tls.ca_file {
        cfg.trust_cert_ca(ca.display());
    }
    // The certificate is checked against the configured name, also through an SSH tunnel.
    cfg.hostname_in_certificate(
        config
            .tls
            .server_name
            .clone()
            .unwrap_or_else(|| host.clone()),
    );
    let (dial_host, dial_port) = match ctx.via {
        Some(a) => (a.ip().to_string(), a.port()),
        None => (host.clone(), *port),
    };
    let tcp = dial(&dial_host, dial_port, ctx.connect_timeout).await?;
    let attempt = tokio::time::timeout(
        ctx.connect_timeout,
        Client::connect(cfg.clone(), tcp.compat_write()),
    )
    .await
    .map_err(|_| Error::timeout(ctx.connect_timeout))?;
    let client = match attempt {
        Ok(c) => c,
        // Azure SQL may send the client to another node; follow it when connecting directly.
        Err(tiberius::error::Error::Routing { host: h, port: p }) if ctx.via.is_none() => {
            cfg.host(&h);
            cfg.port(p);
            let tcp = dial(&h, p, ctx.connect_timeout).await?;
            tokio::time::timeout(
                ctx.connect_timeout,
                Client::connect(cfg, tcp.compat_write()),
            )
            .await
            .map_err(|_| Error::timeout(ctx.connect_timeout))?
            .map_err(|e| error::map(&e, &h, p, None))?
        }
        Err(e) => return Err(error::map(&e, host, *port, None)),
    };
    Ok(MsSqlSession {
        client: Arc::new(Mutex::new(Some(client))),
        host: host.clone(),
        port: *port,
        metadata_timeout: ctx.metadata_timeout,
        analyzer: SqlAnalyzer::new(SqlDialect::MsSql),
        closed: AtomicBool::new(false),
        current: std::sync::Mutex::new(None),
        timed_out: Arc::new(AtomicBool::new(false)),
    })
}

/// Text of any cell in a metadata row.
pub(crate) fn cell(row: &tiberius::Row, i: usize) -> Option<String> {
    let (_, data) = row.cells().nth(i)?;
    match value::convert(data) {
        Value::Null => None,
        Value::Text(s) => Some(s),
        other => Some(treasury_core::format::full_text(&other)),
    }
}

/// An integer cell.
pub(crate) fn int(row: &tiberius::Row, i: usize) -> Option<i64> {
    let (_, data) = row.cells().nth(i)?;
    match value::convert(data) {
        Value::I64(n) => Some(n),
        Value::Decimal(s) | Value::Text(s) => s.split('.').next()?.parse().ok(),
        Value::Bool(b) => Some(i64::from(b)),
        _ => None,
    }
}

impl MsSqlSession {
    pub(crate) fn map(&self, e: &tiberius::error::Error, sql: Option<&str>) -> Error {
        error::map(e, &self.host, self.port, sql)
    }

    fn lost(&self) -> Error {
        Error::network(
            &self.host,
            self.port,
            &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
        )
    }

    pub(crate) fn alive(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.lost());
        }
        Ok(())
    }

    /// Runs a metadata query with `@P1`... parameters under the metadata time limit. On timeout
    /// the request is dropped and the server told to stop with an Attention signal.
    pub(crate) async fn meta(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
    ) -> Result<Vec<tiberius::Row>> {
        self.alive()?;
        let mut guard = self.client.lock().await;
        let client = guard.as_mut().ok_or_else(|| self.lost())?;
        let outcome = tokio::time::timeout(self.metadata_timeout, async {
            client.query(sql, params).await?.into_first_result().await
        })
        .await;
        match outcome {
            Ok(r) => r.map_err(|e| self.map(&e, Some(sql))),
            Err(_) => {
                let _ = client.cancel_query().await;
                Err(Error::timeout(self.metadata_timeout))
            }
        }
    }

    /// How many statements Treasury's splitter finds in `text`.
    pub(crate) fn analyzer_statements(&self, text: &str) -> usize {
        self.analyzer.classify(text).len()
    }

    /// Runs a batch that returns nothing, without a time limit (session settings, edits).
    pub(crate) async fn batch(
        client: &mut Conn,
        sql: &str,
    ) -> std::result::Result<(), tiberius::error::Error> {
        client
            .simple_query(sql)
            .await?
            .into_results()
            .await
            .map(|_| ())
    }
}

/// What the running statement shares with the task that streams it.
struct Run {
    notify: Arc<Notify>,
    timed_out: Arc<AtomicBool>,
    limit: Option<Duration>,
    host: String,
    port: u16,
    sql: String,
    batch: usize,
    tx: mpsc::Sender<Result<ExecEvent>>,
}

enum Outcome {
    Done {
        any_set: bool,
    },
    /// Cancel, the time limit, or the reader going away: the server must be told to stop.
    Interrupted {
        reader_gone: bool,
    },
}

impl Run {
    async fn send(&self, e: Result<ExecEvent>) -> Option<Outcome> {
        tokio::select! {
            biased;
            () = self.notify.notified() => Some(Outcome::Interrupted { reader_gone: false }),
            r = self.tx.send(e) => r.is_err().then_some(Outcome::Interrupted { reader_gone: true }),
        }
    }

    async fn stream(&self, client: &mut Conn, params: &[Param]) -> Result<Outcome> {
        let map =
            |e: &tiberius::error::Error| error::map(e, &self.host, self.port, Some(&self.sql));
        let refs: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
        let started = tokio::select! {
            biased;
            () = self.notify.notified() => return Ok(Outcome::Interrupted { reader_gone: false }),
            s = async {
                if refs.is_empty() {
                    client.simple_query(self.sql.as_str()).await
                } else {
                    client.query(self.sql.as_str(), &refs).await
                }
            } => s,
        };
        let mut items = started.map_err(|e| map(&e))?;
        let mut any_set = false;
        let mut batch = Vec::with_capacity(self.batch);
        loop {
            let item = tokio::select! {
                biased;
                () = self.notify.notified() => return Ok(Outcome::Interrupted { reader_gone: false }),
                item = items.try_next() => item,
            };
            match item {
                Err(e) => {
                    if !batch.is_empty() {
                        let _ = self.tx.send(Ok(ExecEvent::Rows(batch))).await;
                    }
                    return Err(map(&e));
                }
                Ok(None) => break,
                Ok(Some(QueryItem::Metadata(meta))) => {
                    if any_set {
                        if !batch.is_empty()
                            && let Some(o) = self
                                .send(Ok(ExecEvent::Rows(std::mem::take(&mut batch))))
                                .await
                        {
                            return Ok(o);
                        }
                        let end = ExecEvent::ResultEnd {
                            affected: None,
                            truncated: false,
                        };
                        if let Some(o) = self.send(Ok(end)).await {
                            return Ok(o);
                        }
                    }
                    any_set = true;
                    let start =
                        ExecEvent::ResultStart(meta.columns().iter().map(value::column).collect());
                    if let Some(o) = self.send(Ok(start)).await {
                        return Ok(o);
                    }
                }
                Ok(Some(QueryItem::Row(row))) => {
                    batch.push(row.cells().map(|(_, d)| value::convert(d)).collect());
                    if batch.len() >= self.batch
                        && let Some(o) = self
                            .send(Ok(ExecEvent::Rows(std::mem::take(&mut batch))))
                            .await
                    {
                        return Ok(o);
                    }
                }
            }
        }
        if !batch.is_empty()
            && let Some(o) = self.send(Ok(ExecEvent::Rows(batch))).await
        {
            return Ok(o);
        }
        if any_set {
            let end = ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            };
            if let Some(o) = self.send(Ok(end)).await {
                return Ok(o);
            }
        }
        Ok(Outcome::Done { any_set })
    }

    async fn run(&self, client: &mut Conn, params: Vec<Param>) {
        let outcome = self.stream(client, &params).await;
        match outcome {
            Ok(Outcome::Done { any_set: false }) => {
                // A statement without rows: ask how many it changed.
                let affected = async {
                    let rows = client
                        .simple_query("SELECT CAST(@@ROWCOUNT AS bigint)")
                        .await?
                        .into_first_result()
                        .await?;
                    Ok::<_, tiberius::error::Error>(rows.first().and_then(|r| int(r, 0)))
                }
                .await
                .ok()
                .flatten()
                .and_then(|n| u64::try_from(n).ok());
                let _ = self
                    .tx
                    .send(Ok(ExecEvent::ResultEnd {
                        affected,
                        truncated: false,
                    }))
                    .await;
            }
            Ok(Outcome::Done { any_set: true }) => {}
            Ok(Outcome::Interrupted { reader_gone }) => {
                let _ = client.cancel_query().await;
                if !reader_gone {
                    let err = if self.timed_out.load(Ordering::SeqCst) {
                        Error::timeout(self.limit.unwrap_or_default())
                    } else {
                        Error::cancelled()
                    };
                    let _ = self.tx.send(Err(err)).await;
                }
            }
            Err(e) => {
                let _ = self.tx.send(Err(e)).await;
            }
        }
    }
}

#[async_trait]
impl Session for MsSqlSession {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            language: QueryLanguage::Sql(SqlDialect::MsSql),
            introspect: true,
            ddl: true,
            explain: Some(ExplainSupport { analyze: true }),
            cancel: CancelMethod::Native,
            health: true,
            edit: true,
            transactions: true,
            read_only: ReadOnlyEnforcement::Client,
            multiple_results: true,
        }
    }

    fn analyzer(&self) -> &dyn Analyzer {
        &self.analyzer
    }

    async fn ping(&self) -> Result<Duration> {
        let started = Instant::now();
        self.meta("SELECT 1", &[]).await?;
        Ok(started.elapsed())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let rows = self
            .meta(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), \
                        CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), SUSER_SNAME(), DB_NAME()",
                &[],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::protocol("no server facts"))?;
        Ok(ServerInfo {
            product: "SQL Server".into(),
            version: cell(row, 0).unwrap_or_default(),
            details: [
                ("Edition", cell(row, 1)),
                ("Login", cell(row, 2)),
                ("Database", cell(row, 3)),
            ]
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v)))
            .collect(),
        })
    }

    async fn set_read_only(&self, _read_only: bool) -> Result<()> {
        // No server-side switch exists; the guardrails enforce read-only for this engine.
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(n) = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            n.notify_one();
        }
        if let Some(client) = self.client.lock().await.take() {
            let _ = client.close().await;
        }
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
impl Execute for MsSqlSession {
    async fn execute(&self, req: ExecRequest) -> Result<ExecStream> {
        self.alive()?;
        // The analyzer reviews a T-SQL batch as one unit; text with several batches (GO) is
        // refused so that only what was reviewed runs.
        if self.analyzer.classify(&req.text).len() > 1 {
            return Err(Error::unsupported(ONE_AT_A_TIME));
        }
        let mut guard = self.client.clone().lock_owned().await;
        if guard.is_none() {
            return Err(self.lost());
        }
        let notify = Arc::new(Notify::new());
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(notify.clone());
        self.timed_out.store(false, Ordering::SeqCst);
        let timer = req.timeout.map(|limit| {
            let (n, flag) = (notify.clone(), self.timed_out.clone());
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                n.notify_one();
            })
            .abort_handle()
        });
        let (tx, rx) = mpsc::channel(1);
        let run = Run {
            notify,
            timed_out: self.timed_out.clone(),
            limit: req.timeout,
            host: self.host.clone(),
            port: self.port,
            sql: req.text.clone(),
            batch: usize::try_from(req.fetch_rows.max(1)).unwrap_or(500),
            tx,
        };
        let params: Vec<Param> = req
            .params
            .iter()
            .map(|p| value::to_param(&p.value))
            .collect();
        tokio::spawn(async move {
            if let Some(client) = guard.as_mut() {
                run.run(client, params).await;
            }
            if let Some(t) = timer {
                t.abort();
            }
        });
        Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }
}

#[async_trait]
impl Cancel for MsSqlSession {
    async fn cancel(&self) -> Result<()> {
        if let Some(n) = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            n.notify_one();
        }
        Ok(())
    }
}
