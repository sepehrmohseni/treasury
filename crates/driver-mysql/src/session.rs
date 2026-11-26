//! One MySQL or MariaDB connection.

use crate::error;
use crate::tls;
use crate::value::{self, Shape};
use async_trait::async_trait;
use futures::stream;
use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Conn, DriverError, Opts, OptsBuilder, Params, QueryResult, SslOpts, Statement};
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use treasury_core::analyze::Analyzer;
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, SecretSlot, TlsMode};
use treasury_core::driver::{
    Cancel, CancelMethod, Capabilities, ConnectContext, Edit, Execute, Explain, ExplainSupport,
    Health, Introspect, QueryLanguage, ReadOnlyEnforcement, ServerInfo, Session, SqlDialect,
};
use treasury_core::exec::{ExecEvent, ExecRequest, ExecStream, Notice, NoticeLevel};
use treasury_core::{Error, ErrorKind, Result};
use treasury_sql::SqlAnalyzer;

/// How long a `KILL QUERY` connection may take.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// MySQL error code for commands the prepared-statement protocol does not support.
const ER_UNSUPPORTED_PS: u16 = 1295;

const ONE_AT_A_TIME: &str =
    "This text holds more than one statement. Treasury runs them one at a time.";

/// An open MySQL or MariaDB session.
pub struct MySqlSession {
    pub(crate) conn: Arc<Mutex<Option<Conn>>>,
    opts: Opts,
    id: u32,
    pub(crate) engine: Engine,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) metadata_timeout: Duration,
    analyzer: SqlAnalyzer,
    closed: AtomicBool,
    /// Set when the running statement is cancelled or times out.
    stop: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
}

/// Opens a connection.
pub(crate) async fn connect(
    engine: Engine,
    config: &ConnectionConfig,
    ctx: ConnectContext,
) -> Result<MySqlSession> {
    let Endpoint::Host { host, port } = &config.endpoint else {
        return Err(Error::config(
            Some("host"),
            "MySQL and MariaDB connections need a host and port.",
        ));
    };
    let (dial_host, dial_port) = match ctx.via {
        Some(a) => (a.ip().to_string(), a.port()),
        None => (host.clone(), *port),
    };
    let password = ctx.secrets.secret(SecretSlot::Password)?;
    let passphrase = ctx.secrets.secret(SecretSlot::KeyPassphrase)?;
    let mut attrs = HashMap::new();
    attrs.insert("program_name".to_owned(), "Treasury".to_owned());
    let base = OptsBuilder::default()
        .ip_or_hostname(dial_host)
        .tcp_port(dial_port)
        .user(Some(
            config.user.clone().unwrap_or_else(|| "root".to_owned()),
        ))
        .pass(password.map(|p| p.expose_secret().to_owned()))
        .db_name(config.database.clone())
        .stmt_cache_size(0)
        // UPDATE reports the rows it matched, so an edit that changes nothing still counts.
        .client_found_rows(true)
        .tcp_nodelay(true)
        .connect_attributes(attrs)
        .init(vec!["SET time_zone = '+00:00'"]);
    let ssl = match config.tls.mode {
        TlsMode::Disable => None,
        TlsMode::Prefer | TlsMode::Require => {
            Some(tls::ssl_opts(&config.tls, host, passphrase.as_ref())?)
        }
    };
    let open = |ssl: Option<SslOpts>| {
        let opts: Opts = base.clone().ssl_opts(ssl).into();
        let limit = ctx.connect_timeout;
        async move {
            match tokio::time::timeout(limit, Conn::new(opts.clone())).await {
                Ok(r) => (r, opts),
                Err(_) => (
                    Err(mysql_async::Error::Io(mysql_async::IoError::Io(
                        std::io::Error::from(std::io::ErrorKind::TimedOut),
                    ))),
                    opts,
                ),
            }
        }
    };
    let (mut result, mut opts) = open(ssl).await;
    if config.tls.mode == TlsMode::Prefer
        && matches!(
            result,
            Err(mysql_async::Error::Driver(
                DriverError::NoClientSslFlagFromServer
            ))
        )
    {
        (result, opts) = open(None).await;
    }
    let mut conn = result.map_err(|e| error::map(&e, host, *port, None))?;
    let id = conn.id();
    if ctx.read_only {
        conn.query_drop("SET SESSION TRANSACTION READ ONLY")
            .await
            .map_err(|e| error::map(&e, host, *port, None))?;
    }
    let dialect = if engine == Engine::MariaDb {
        SqlDialect::MariaDb
    } else {
        SqlDialect::MySql
    };
    Ok(MySqlSession {
        conn: Arc::new(Mutex::new(Some(conn))),
        opts,
        id,
        engine,
        host: host.clone(),
        port: *port,
        metadata_timeout: ctx.metadata_timeout,
        analyzer: SqlAnalyzer::new(dialect),
        closed: AtomicBool::new(false),
        stop: Arc::new(AtomicBool::new(false)),
        timed_out: Arc::new(AtomicBool::new(false)),
    })
}

/// Stops the statement running on connection `id` from a second connection.
async fn kill(opts: &Opts, id: u32) -> std::result::Result<(), mysql_async::Error> {
    let work = async {
        let mut c = Conn::new(opts.clone()).await?;
        c.query_drop(format!("KILL QUERY {id}")).await?;
        c.disconnect().await
    };
    match tokio::time::timeout(KILL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(mysql_async::Error::Io(mysql_async::IoError::Io(
            std::io::Error::from(std::io::ErrorKind::TimedOut),
        ))),
    }
}

/// Text of any value in a metadata row.
pub(crate) fn cell(row: &mysql_async::Row, i: usize) -> Option<String> {
    use mysql_async::Value as My;
    match row.as_ref(i)? {
        My::NULL => None,
        My::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        My::Int(n) => Some(n.to_string()),
        My::UInt(n) => Some(n.to_string()),
        My::Float(f) => Some(f.to_string()),
        My::Double(f) => Some(f.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_owned()),
    }
}

/// An integer in a metadata row.
pub(crate) fn int(row: &mysql_async::Row, i: usize) -> Option<i64> {
    cell(row, i).and_then(|s| s.parse().ok())
}

impl MySqlSession {
    pub(crate) fn map(&self, e: &mysql_async::Error, sql: Option<&str>) -> Error {
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

    pub(crate) fn dialect(&self) -> SqlDialect {
        if self.engine == Engine::MariaDb {
            SqlDialect::MariaDb
        } else {
            SqlDialect::MySql
        }
    }

    /// Runs a metadata query with bind parameters under the metadata time limit. On timeout the
    /// statement is killed and awaited, so the connection is left in a clean state.
    pub(crate) async fn meta(
        &self,
        sql: &str,
        params: Vec<mysql_async::Value>,
    ) -> Result<Vec<mysql_async::Row>> {
        self.alive()?;
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| self.lost())?;
        let params = if params.is_empty() {
            Params::Empty
        } else {
            Params::Positional(params)
        };
        let mut work = Box::pin(conn.exec::<mysql_async::Row, _, _>(sql, params));
        match tokio::time::timeout(self.metadata_timeout, &mut work).await {
            Ok(r) => r.map_err(|e| self.map(&e, Some(sql))),
            Err(_) => {
                let _ = kill(&self.opts, self.id).await;
                let _ = work.await;
                Err(Error::timeout(self.metadata_timeout))
            }
        }
    }

    /// Like [`Self::meta`] for statements the prepared protocol does not take (some SHOW forms).
    pub(crate) async fn meta_text(&self, sql: &str) -> Result<Vec<mysql_async::Row>> {
        self.alive()?;
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| self.lost())?;
        let mut work = Box::pin(conn.query::<mysql_async::Row, _>(sql));
        match tokio::time::timeout(self.metadata_timeout, &mut work).await {
            Ok(r) => r.map_err(|e| self.map(&e, Some(sql))),
            Err(_) => {
                let _ = kill(&self.opts, self.id).await;
                let _ = work.await;
                Err(Error::timeout(self.metadata_timeout))
            }
        }
    }

    /// Runs statements on the session's connection without a time limit (edits).
    pub(crate) async fn with_conn<T>(
        &self,
        f: impl for<'c> FnOnce(&'c mut Conn) -> futures::future::BoxFuture<'c, Result<T>>,
    ) -> Result<T> {
        self.alive()?;
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| self.lost())?;
        f(conn).await
    }
}

/// What a running statement needs besides the connection.
struct Run {
    opts: Opts,
    id: u32,
    stop: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    limit: Option<Duration>,
    host: String,
    port: u16,
    sql: String,
    batch: usize,
    json_binary: bool,
    tx: mpsc::Sender<Result<ExecEvent>>,
}

enum Pumped {
    /// Every result set was read; the server reported this many warnings.
    Done(u32),
    /// The reader went away; the statement was stopped.
    Stopped,
}

impl Run {
    fn map(&self, e: &mysql_async::Error) -> Error {
        let err = error::map(e, &self.host, self.port, Some(&self.sql));
        if err.kind == ErrorKind::Cancelled && self.timed_out.load(Ordering::SeqCst) {
            return Error::timeout(self.limit.unwrap_or_default());
        }
        err
    }

    async fn send(&self, e: Result<ExecEvent>) -> bool {
        self.tx.send(e).await.is_ok()
    }

    /// Stops a statement whose reader is gone and reads what the server still sends.
    async fn abandon<P: Protocol>(&self, result: &mut QueryResult<'_, '_, P>) -> Pumped {
        self.stop.store(true, Ordering::SeqCst);
        let _ = kill(&self.opts, self.id).await;
        while let Ok(Some(_)) = result.next().await {}
        while !result.is_empty() {
            if result.next().await.is_err() {
                break;
            }
        }
        Pumped::Stopped
    }

    async fn pump<P: Protocol>(&self, result: &mut QueryResult<'_, '_, P>) -> Result<Pumped> {
        let mut warnings = 0u32;
        loop {
            match result.columns().filter(|c| !c.is_empty()) {
                Some(cols) => {
                    let shapes: Vec<Shape> = cols.iter().map(value::shape).collect();
                    let start = ExecEvent::ResultStart(cols.iter().map(value::column).collect());
                    if !self.send(Ok(start)).await {
                        return Ok(self.abandon(result).await);
                    }
                    let mut batch = Vec::with_capacity(self.batch);
                    loop {
                        match result.next().await {
                            Ok(Some(row)) => {
                                batch.push(
                                    row.unwrap()
                                        .into_iter()
                                        .zip(&shapes)
                                        .map(|(v, s)| value::convert(*s, self.json_binary, v))
                                        .collect(),
                                );
                                if batch.len() >= self.batch
                                    && !self
                                        .send(Ok(ExecEvent::Rows(std::mem::take(&mut batch))))
                                        .await
                                {
                                    return Ok(self.abandon(result).await);
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                if !batch.is_empty() {
                                    let _ = self.send(Ok(ExecEvent::Rows(batch))).await;
                                }
                                return Err(self.map(&e));
                            }
                        }
                    }
                    if !batch.is_empty() && !self.send(Ok(ExecEvent::Rows(batch))).await {
                        return Ok(self.abandon(result).await);
                    }
                    let end = ExecEvent::ResultEnd {
                        affected: None,
                        truncated: false,
                    };
                    if !self.send(Ok(end)).await {
                        return Ok(self.abandon(result).await);
                    }
                }
                None => {
                    // A statement without rows: reading moves past its OK packet.
                    result.next().await.map_err(|e| self.map(&e))?;
                    let end = ExecEvent::ResultEnd {
                        affected: Some(result.affected_rows()),
                        truncated: false,
                    };
                    if !self.send(Ok(end)).await {
                        return Ok(self.abandon(result).await);
                    }
                }
            }
            warnings += u32::from(result.warnings());
            if result.is_empty() {
                return Ok(Pumped::Done(warnings));
            }
        }
    }

    async fn run(&self, conn: &mut Conn, stmt: Option<&Statement>, params: Params) -> Result<()> {
        let pumped = match stmt {
            Some(s) => {
                let mut r = conn.exec_iter(s, params).await.map_err(|e| self.map(&e))?;
                self.pump(&mut r).await?
            }
            None => {
                let mut r = conn
                    .query_iter(self.sql.as_str())
                    .await
                    .map_err(|e| self.map(&e))?;
                self.pump(&mut r).await?
            }
        };
        let Pumped::Done(warnings) = pumped else {
            return Ok(());
        };
        // SLEEP() and some other statements end normally when killed; the user still asked
        // for them to stop.
        if self.stop.load(Ordering::SeqCst) {
            return Err(if self.timed_out.load(Ordering::SeqCst) {
                Error::timeout(self.limit.unwrap_or_default())
            } else {
                Error::cancelled()
            });
        }
        if warnings > 0
            && let Ok(rows) = conn.query::<mysql_async::Row, _>("SHOW WARNINGS").await
        {
            for row in rows {
                let level = cell(&row, 0).unwrap_or_default();
                let notice = Notice {
                    level: if level.eq_ignore_ascii_case("note") {
                        NoticeLevel::Info
                    } else {
                        NoticeLevel::Warning
                    },
                    message: cell(&row, 2).unwrap_or_default(),
                    code: cell(&row, 1),
                };
                if !self.send(Ok(ExecEvent::Notice(notice))).await {
                    break;
                }
            }
        }
        Ok(())
    }
}

fn params(req: &ExecRequest) -> Params {
    if req.params.is_empty() {
        return Params::Empty;
    }
    if req.params.iter().all(|p| p.name.is_some()) {
        return Params::Named(
            req.params
                .iter()
                .map(|p| {
                    let name = p.name.clone().unwrap_or_default();
                    let name = name.trim_start_matches(':').as_bytes().to_vec();
                    (name, value::to_mysql(&p.value))
                })
                .collect(),
        );
    }
    Params::Positional(
        req.params
            .iter()
            .map(|p| value::to_mysql(&p.value))
            .collect(),
    )
}

#[async_trait]
impl Session for MySqlSession {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            language: QueryLanguage::Sql(self.dialect()),
            introspect: true,
            ddl: true,
            explain: Some(ExplainSupport { analyze: true }),
            cancel: CancelMethod::KillFromSecondConnection,
            health: true,
            edit: true,
            transactions: true,
            read_only: ReadOnlyEnforcement::Server,
            multiple_results: true,
        }
    }

    fn analyzer(&self) -> &dyn Analyzer {
        &self.analyzer
    }

    async fn ping(&self) -> Result<Duration> {
        self.alive()?;
        let started = Instant::now();
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| self.lost())?;
        match tokio::time::timeout(Duration::from_secs(10), conn.ping()).await {
            Ok(r) => r.map_err(|e| self.map(&e, None))?,
            Err(_) => return Err(Error::timeout(Duration::from_secs(10))),
        }
        Ok(started.elapsed())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let rows = self
            .meta(
                "SELECT VERSION(), CURRENT_USER(), DATABASE(), @@read_only, @@version_comment",
                vec![],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::protocol("no server facts"))?;
        let version = cell(row, 0).unwrap_or_default();
        let comment = cell(row, 4).unwrap_or_default();
        let product = if version.contains("MariaDB") || comment.contains("MariaDB") {
            "MariaDB"
        } else {
            "MySQL"
        };
        let mut details = vec![("User".to_owned(), cell(row, 1).unwrap_or_default())];
        if let Some(db) = cell(row, 2) {
            details.push(("Database".to_owned(), db));
        }
        if int(row, 3) == Some(1) {
            details.push(("Server".to_owned(), "read-only".to_owned()));
        }
        Ok(ServerInfo {
            product: product.into(),
            version,
            details,
        })
    }

    async fn set_read_only(&self, read_only: bool) -> Result<()> {
        let sql = if read_only {
            "SET SESSION TRANSACTION READ ONLY"
        } else {
            "SET SESSION TRANSACTION READ WRITE"
        };
        self.meta_text(sql).await.map(|_| ())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        let _ = kill(&self.opts, self.id).await;
        if let Some(conn) = self.conn.lock().await.take() {
            let _ = conn.disconnect().await;
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
impl Execute for MySqlSession {
    async fn execute(&self, req: ExecRequest) -> Result<ExecStream> {
        self.alive()?;
        let mut guard = self.conn.clone().lock_owned().await;
        let conn = guard.as_mut().ok_or_else(|| self.lost())?;
        // Preparing proves the text is one statement; the protocol would run several.
        let stmt = match conn.prep(req.text.as_str()).await {
            Ok(s) => Some(s),
            Err(mysql_async::Error::Server(e)) if e.code == ER_UNSUPPORTED_PS => {
                if self.analyzer.classify(&req.text).len() != 1 {
                    return Err(Error::unsupported(ONE_AT_A_TIME));
                }
                if !req.params.is_empty() {
                    return Err(Error::unsupported(
                        "This kind of statement cannot take parameters. Write the values in it.",
                    ));
                }
                None
            }
            Err(e) => {
                let err = self.map(&e, Some(&req.text));
                // A second statement makes the text a syntax error for the server.
                if err.kind == ErrorKind::Syntax && self.analyzer.classify(&req.text).len() > 1 {
                    return Err(Error::unsupported(ONE_AT_A_TIME));
                }
                return Err(err);
            }
        };
        self.stop.store(false, Ordering::SeqCst);
        self.timed_out.store(false, Ordering::SeqCst);
        let timer = req.timeout.map(|limit| {
            let (opts, id) = (self.opts.clone(), self.id);
            let (stop, flag) = (self.stop.clone(), self.timed_out.clone());
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                stop.store(true, Ordering::SeqCst);
                let _ = kill(&opts, id).await;
            })
            .abort_handle()
        });
        let (tx, rx) = mpsc::channel(1);
        let run = Run {
            opts: self.opts.clone(),
            id: self.id,
            stop: self.stop.clone(),
            timed_out: self.timed_out.clone(),
            limit: req.timeout,
            host: self.host.clone(),
            port: self.port,
            sql: req.text.clone(),
            batch: usize::try_from(req.fetch_rows.max(1)).unwrap_or(500),
            json_binary: self.engine == Engine::MySql,
            tx,
        };
        let params = params(&req);
        tokio::spawn(async move {
            let Some(conn) = guard.as_mut() else {
                return;
            };
            let outcome = run.run(conn, stmt.as_ref(), params).await;
            if let Some(t) = timer {
                t.abort();
            }
            if let Some(s) = stmt {
                let _ = conn.close(s).await;
            }
            if let Err(e) = outcome {
                let _ = run.tx.send(Err(e)).await;
            }
        });
        Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }
}

#[async_trait]
impl Cancel for MySqlSession {
    async fn cancel(&self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        kill(&self.opts, self.id)
            .await
            .map_err(|e| self.map(&e, None))
    }
}
