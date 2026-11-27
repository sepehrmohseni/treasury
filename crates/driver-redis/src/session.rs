//! One Redis or Valkey connection: a multiplexed connection speaking RESP3, to one server or to
//! the master a Sentinel names.
//!
//! Commands run one at a time. Replies that can be large (LRANGE, HGETALL, SMEMBERS, ZRANGE by
//! position) are read in pages (LRANGE by index, HSCAN, SSCAN), so a list of a million entries
//! streams instead of arriving in one piece. A blocking command is cancelled from a second
//! connection with CLIENT UNBLOCK; anything else that runs long with CLIENT KILL, after which the
//! connection opens again.

use crate::error;
use crate::lang::{self, CommandTable, RedisAnalyzer};
use crate::value::{self, Shape};
use async_trait::async_trait;
use futures::stream;
use redis::aio::MultiplexedConnection;
use redis::{
    AsyncConnectionConfig, Client, ConnectionAddr, IntoConnectionInfo, ProtocolVersion,
    RedisConnectionInfo, Value as R,
};
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use treasury_core::analyze::{Analyzer, StatementClass};
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, SecretSlot, TlsMode};
use treasury_core::driver::{
    Cancel, CancelMethod, Capabilities, ConnectContext, Execute, Introspect, QueryLanguage,
    ReadOnlyEnforcement, ServerInfo, Session,
};
use treasury_core::exec::{ExecEvent, ExecRequest, ExecStream};
use treasury_core::{Error, ErrorKind, Result};
use treasury_tunnel::TlsBridge;

/// Entries read per page of a large reply.
const PAGE: usize = 1_000;

/// An open Redis session.
pub struct RedisSession {
    pub(crate) engine: Engine,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) client: Client,
    conn: tokio::sync::Mutex<MultiplexedConnection>,
    /// The server's id for `conn`, for CLIENT UNBLOCK and CLIENT KILL.
    id: Arc<AtomicI64>,
    /// The database commands run in, unless one SELECTs another.
    pub(crate) db: i64,
    /// Connections to other databases, for the key browser.
    db_conns: tokio::sync::Mutex<HashMap<i64, MultiplexedConnection>>,
    pub(crate) delimiter: String,
    pub(crate) metadata_timeout: Duration,
    connect_timeout: Duration,
    analyzer: RedisAnalyzer,
    run_id: Option<String>,
    version: String,
    mode: &'static str,
    closed: AtomicBool,
    /// CLIENT KILL ended the connection; the next command opens it again.
    broken: Arc<AtomicBool>,
    /// A cancel or time limit stopped the command running now.
    stopped: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    _bridge: Option<TlsBridge>,
}

/// The server's id for `conn`.
async fn client_id(conn: &mut MultiplexedConnection) -> redis::RedisResult<i64> {
    redis::cmd("CLIENT").arg("ID").query_async(conn).await
}

/// `COMMAND`'s reply as a table of ACL categories, subcommands included.
fn command_table(reply: &R) -> CommandTable {
    fn text(v: &R) -> Option<String> {
        match v {
            R::BulkString(b) => String::from_utf8(b.clone()).ok(),
            R::SimpleString(s) => Some(s.clone()),
            R::VerbatimString { text, .. } => Some(text.clone()),
            _ => None,
        }
    }
    fn items(v: &R) -> &[R] {
        match v {
            R::Array(a) | R::Set(a) => a,
            _ => &[],
        }
    }
    fn walk(entry: &R, out: &mut Vec<(String, Vec<String>)>) {
        let fields = items(entry);
        let Some(name) = fields.first().and_then(text) else {
            return;
        };
        let categories = fields
            .get(6)
            .map(|c| items(c).iter().filter_map(text).collect())
            .unwrap_or_default();
        out.push((name, categories));
        for sub in fields.get(9).map(items).unwrap_or_default() {
            walk(sub, out);
        }
    }
    let mut entries = Vec::new();
    for e in items(reply) {
        walk(e, &mut entries);
    }
    CommandTable::new(entries)
}

/// The value of `field` in INFO's text.
pub(crate) fn info_field<'a>(info: &'a str, field: &str) -> Option<&'a str> {
    info.lines()
        .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
        .map(str::trim)
}

fn settings(config: &ConnectionConfig, password: Option<String>, db: i64) -> RedisConnectionInfo {
    let mut s = RedisConnectionInfo::default()
        .set_protocol(ProtocolVersion::RESP3)
        .set_db(db)
        .set_lib_name("treasury", env!("CARGO_PKG_VERSION"));
    if let Some(user) = config.user.as_deref().filter(|u| !u.is_empty()) {
        s = s.set_username(user);
    }
    if let Some(p) = password {
        s = s.set_password(p);
    }
    s
}

pub(crate) async fn connect(
    engine: Engine,
    config: &ConnectionConfig,
    ctx: ConnectContext,
) -> Result<RedisSession> {
    let db: i64 = match config.database.as_deref().map(str::trim) {
        None | Some("") => 0,
        Some(d) => d.trim_start_matches("db").parse().map_err(|_| {
            Error::config(Some("database"), "A Redis database is a number, such as 0.")
        })?,
    };
    let password = ctx
        .secrets
        .secret(SecretSlot::Password)?
        .map(|p| p.expose_secret().to_owned());
    let redis_settings = settings(config, password, db);
    let sentinel = config.options.get("mode").map(String::as_str) == Some("sentinel");
    let tls_on = config.tls.mode != TlsMode::Disable;
    let (host, port, client, bridge, mode) = if sentinel {
        let hosts = match &config.endpoint {
            Endpoint::Hosts { hosts } if !hosts.is_empty() => hosts.clone(),
            Endpoint::Host { host, port } => vec![(host.clone(), *port)],
            _ => return Err(Error::config(Some("host"), "Enter the Sentinel hosts.")),
        };
        if ctx.via.is_some() {
            return Err(Error::unsupported(
                "Sentinel hands out the addresses of its servers, which an SSH tunnel to one host \
                 cannot reach. Connect to the master directly through the tunnel instead.",
            ));
        }
        let master = config.options.get("master_name").cloned().ok_or_else(|| {
            Error::config(
                Some("master"),
                "Enter the name the Sentinels know the master by.",
            )
        })?;
        let node = redis::sentinel::SentinelNodeConnectionInfo::default()
            .set_redis_connection_info(redis_settings.clone());
        let node = if tls_on {
            node.set_tls_mode(if config.tls.accept_invalid {
                redis::TlsMode::Insecure
            } else {
                redis::TlsMode::Secure
            })
        } else {
            node
        };
        let sentinels: Vec<ConnectionAddr> = hosts
            .iter()
            .map(|(h, p)| ConnectionAddr::Tcp(h.clone(), *p))
            .collect();
        let mut sc = redis::sentinel::SentinelClient::build(
            sentinels,
            master,
            Some(node),
            redis::sentinel::SentinelServerType::Master,
        )
        .map_err(|e| error::map(&e, &hosts[0].0, hosts[0].1))?;
        let client = tokio::time::timeout(ctx.connect_timeout, sc.async_get_client())
            .await
            .map_err(|_| Error::timeout(ctx.connect_timeout))?
            .map_err(|e| error::map(&e, &hosts[0].0, hosts[0].1))?;
        let (h, p) = match client.get_connection_info().addr() {
            ConnectionAddr::Tcp(h, p)
            | ConnectionAddr::TcpTls {
                host: h, port: p, ..
            } => (h.clone(), *p),
            _ => (hosts[0].0.clone(), hosts[0].1),
        };
        (h, p, client, None, "sentinel")
    } else {
        let Endpoint::Host { host, port } = &config.endpoint else {
            return Err(Error::config(
                Some("host"),
                "A Redis connection needs a host and port.",
            ));
        };
        let (host, port) = (host.clone(), *port);
        // One server: Treasury makes the TLS itself through a bridge, so the certificate is
        // checked with the shared rules (the platform's verifier, a custom CA, client keys).
        let mut bridge = None;
        if tls_on {
            let upstream = match ctx.via {
                Some(a) => a,
                None => tokio::net::lookup_host((host.as_str(), port))
                    .await
                    .map_err(|e| Error::network(&host, port, &e))?
                    .next()
                    .ok_or_else(|| {
                        Error::network(
                            &host,
                            port,
                            &std::io::Error::from(std::io::ErrorKind::NotFound),
                        )
                    })?,
            };
            let passphrase = ctx.secrets.secret(SecretSlot::KeyPassphrase)?;
            let rustls = treasury_tunnel::tls::client_config(&config.tls, passphrase.as_ref())?;
            let name = treasury_tunnel::tls::server_name(&config.tls, &host)?;
            match TlsBridge::probe(upstream, rustls.clone(), name.clone(), &host, port).await {
                Ok(()) => bridge = Some(TlsBridge::start(upstream, rustls, name).await?),
                Err(e)
                    if config.tls.mode == TlsMode::Prefer
                        && !matches!(e.kind, ErrorKind::Network { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        let addr = match (&bridge, ctx.via) {
            (Some(b), _) => {
                ConnectionAddr::Tcp(b.local_addr().ip().to_string(), b.local_addr().port())
            }
            (None, Some(a)) => ConnectionAddr::Tcp(a.ip().to_string(), a.port()),
            (None, None) => ConnectionAddr::Tcp(host.clone(), port),
        };
        let info = addr
            .into_connection_info()
            .map_err(|e| error::map(&e, &host, port))?
            .set_redis_settings(redis_settings);
        let client = Client::open(info).map_err(|e| error::map(&e, &host, port))?;
        (host, port, client, bridge, "standalone")
    };
    let options = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(ctx.connect_timeout))
        .set_response_timeout(None);
    let mut conn = tokio::time::timeout(
        ctx.connect_timeout + Duration::from_secs(1),
        client.get_multiplexed_async_connection_with_config(&options),
    )
    .await
    .map_err(|_| Error::timeout(ctx.connect_timeout))?
    .map_err(|e| error::map(&e, &host, port))?;
    // A user without CLIENT ID still connects; cancelling then has nothing to name.
    let id = client_id(&mut conn).await.unwrap_or(0);
    // The server's own description of its commands decides their classes; a user who may
    // not run COMMAND gets the built-in table.
    let table = redis::cmd("COMMAND")
        .query_async::<R>(&mut conn)
        .await
        .map(|r| command_table(&r))
        .unwrap_or_default();
    let info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(&mut conn)
        .await
        .unwrap_or_default();
    let version = info_field(&info, "valkey_version")
        .or_else(|| info_field(&info, "redis_version"))
        .unwrap_or("")
        .to_owned();
    Ok(RedisSession {
        engine,
        host,
        port,
        client,
        conn: tokio::sync::Mutex::new(conn),
        id: Arc::new(AtomicI64::new(id)),
        db,
        db_conns: tokio::sync::Mutex::new(HashMap::new()),
        delimiter: config
            .options
            .get("delimiter")
            .cloned()
            .unwrap_or_else(|| ":".into()),
        metadata_timeout: ctx.metadata_timeout,
        connect_timeout: ctx.connect_timeout,
        analyzer: RedisAnalyzer::new(table),
        run_id: info_field(&info, "run_id").map(str::to_owned),
        version,
        mode,
        closed: AtomicBool::new(false),
        broken: Arc::new(AtomicBool::new(false)),
        stopped: Arc::new(AtomicBool::new(false)),
        timed_out: Arc::new(AtomicBool::new(false)),
        _bridge: bridge,
    })
}

impl RedisSession {
    pub(crate) fn map(&self, e: &redis::RedisError) -> Error {
        error::map(e, &self.host, self.port)
    }

    fn alive(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::network(
                &self.host,
                self.port,
                &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
            ));
        }
        Ok(())
    }

    async fn open(&self) -> Result<MultiplexedConnection> {
        let options = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(self.connect_timeout))
            .set_response_timeout(None);
        self.client
            .get_multiplexed_async_connection_with_config(&options)
            .await
            .map_err(|e| self.map(&e))
    }

    /// The connection commands run on.
    pub(crate) async fn conn(&self) -> MultiplexedConnection {
        self.conn.lock().await.clone()
    }

    /// Opens the connection again, after CLIENT KILL broke it.
    async fn reopen(&self) -> Result<()> {
        let mut fresh = self.open().await?;
        let id = client_id(&mut fresh).await.unwrap_or(0);
        self.id.store(id, Ordering::SeqCst);
        *self.conn.lock().await = fresh;
        Ok(())
    }

    /// A connection to database `db`, for the key browser: never the one commands run on,
    /// which a SELECT typed in the editor moves to another database.
    pub(crate) async fn db_conn(&self, db: i64) -> Result<MultiplexedConnection> {
        let mut conns = self.db_conns.lock().await;
        if let Some(c) = conns.get(&db) {
            return Ok(c.clone());
        }
        let mut c = self.open().await?;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut c)
            .await
            .map_err(|e| self.map(&e))?;
        conns.insert(db, c.clone());
        Ok(c)
    }

    /// Runs a command for metadata under the metadata time limit.
    pub(crate) async fn meta<T: redis::FromRedisValue>(
        &self,
        conn: &mut MultiplexedConnection,
        cmd: &redis::Cmd,
    ) -> Result<T> {
        self.alive()?;
        tokio::time::timeout(self.metadata_timeout, cmd.query_async(conn))
            .await
            .map_err(|_| Error::timeout(self.metadata_timeout))?
            .map_err(|e| self.map(&e))
    }

    fn side(&self) -> SideCancel {
        SideCancel {
            client: self.client.clone(),
            id: self.id.clone(),
            timeout: self.connect_timeout,
            broken: self.broken.clone(),
        }
    }
}

/// A command whose reply can be large, read in pages.
#[derive(Debug, PartialEq)]
enum Paged {
    /// LRANGE key start stop.
    List { key: String, start: i64, stop: i64 },
    /// HGETALL key, as HSCAN.
    Hash { key: String },
    /// SMEMBERS key, as SSCAN.
    Set { key: String },
    /// ZRANGE key start stop [WITHSCORES], by position.
    Zset {
        key: String,
        start: i64,
        stop: i64,
        scores: bool,
    },
}

fn paged(argv: &[String]) -> Option<Paged> {
    let name = argv.first()?.to_ascii_lowercase();
    let int = |i: usize| argv.get(i)?.parse::<i64>().ok();
    match (name.as_str(), argv.len()) {
        ("lrange", 4) => Some(Paged::List {
            key: argv[1].clone(),
            start: int(2)?,
            stop: int(3)?,
        }),
        ("hgetall", 2) => Some(Paged::Hash {
            key: argv[1].clone(),
        }),
        ("smembers", 2) => Some(Paged::Set {
            key: argv[1].clone(),
        }),
        ("zrange", 4 | 5) => {
            let scores = argv.len() == 5;
            if scores && !argv[4].eq_ignore_ascii_case("withscores") {
                return None;
            }
            Some(Paged::Zset {
                key: argv[1].clone(),
                start: int(2)?,
                stop: int(3)?,
                scores,
            })
        }
        _ => None,
    }
}

/// Absolute `start..=stop` of a list or sorted set of `len` entries, as Redis reads negative
/// positions; `None` when empty.
fn bounds(start: i64, stop: i64, len: i64) -> Option<(i64, i64)> {
    let at = |i: i64| if i < 0 { (len + i).max(0) } else { i };
    let (s, e) = (at(start), at(stop).min(len - 1));
    (s <= e && len > 0).then_some((s, e))
}

/// What the task running one command needs.
struct Run {
    conn: MultiplexedConnection,
    argv: Vec<String>,
    writes: bool,
    host: String,
    port: u16,
    stopped: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    limit: Option<Duration>,
    tx: mpsc::Sender<Result<ExecEvent>>,
}

impl Run {
    fn map(&self, e: &redis::RedisError) -> Error {
        let err = error::map(e, &self.host, self.port);
        if self.stopped.load(Ordering::SeqCst) {
            return self.stop_error();
        }
        err
    }

    fn stop_error(&self) -> Error {
        if self.timed_out.load(Ordering::SeqCst) {
            Error::timeout(self.limit.unwrap_or_default())
        } else {
            Error::cancelled()
        }
    }

    async fn send(&self, e: Result<ExecEvent>) -> bool {
        self.tx.send(e).await.is_ok()
    }

    async fn query<T: redis::FromRedisValue>(&self, cmd: &redis::Cmd) -> Result<T> {
        let mut conn = self.conn.clone();
        cmd.query_async(&mut conn).await.map_err(|e| self.map(&e))
    }

    async fn run(self) {
        let outcome = match paged(&self.argv) {
            Some(p) => self.pages(p).await,
            None => self.single().await,
        };
        let outcome = match outcome {
            Ok(()) if self.stopped.load(Ordering::SeqCst) => Err(self.stop_error()),
            other => other,
        };
        if let Err(e) = outcome {
            let _ = self.tx.send(Err(e)).await;
        }
    }

    async fn single(&self) -> Result<()> {
        let mut cmd = redis::cmd(&self.argv[0]);
        for a in &self.argv[1..] {
            cmd.arg(a);
        }
        let reply: R = self.query(&cmd).await?;
        if let R::ServerError(e) = &reply {
            return Err(Error::new(ErrorKind::Server, e.to_string()));
        }
        let end = |affected| ExecEvent::ResultEnd {
            affected,
            truncated: false,
        };
        // A write that answers with a number says how many keys or entries it changed.
        if let (true, R::Int(n)) = (self.writes, &reply) {
            let _ = self.send(Ok(end(u64::try_from(*n).ok()))).await;
            return Ok(());
        }
        match value::shape(&reply) {
            Shape::Done => {
                let _ = self.send(Ok(end(None))).await;
            }
            Shape::Rows { columns, rows } => {
                if self.send(Ok(ExecEvent::ResultStart(columns))).await
                    && self.send(Ok(ExecEvent::Rows(rows))).await
                {
                    let _ = self.send(Ok(end(None))).await;
                }
            }
        }
        Ok(())
    }

    /// Sends one page; the columns go first, from the first page's values.
    async fn page(
        &self,
        names: &[&str],
        rows: Vec<Vec<treasury_core::Value>>,
        first: &mut bool,
    ) -> bool {
        if *first {
            *first = false;
            let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
            if !self
                .send(Ok(ExecEvent::ResultStart(value::columns(&names, &rows))))
                .await
            {
                return false;
            }
        }
        rows.is_empty() || self.send(Ok(ExecEvent::Rows(rows))).await
    }

    async fn pages(&self, p: Paged) -> Result<()> {
        let mut first = true;
        match &p {
            Paged::List { key, start, stop }
            | Paged::Zset {
                key, start, stop, ..
            } => {
                let (len_cmd, range_cmd, names): (&str, &str, &[&str]) = match &p {
                    Paged::Zset { scores: true, .. } => ("ZCARD", "ZRANGE", &["member", "score"]),
                    Paged::Zset { .. } => ("ZCARD", "ZRANGE", &["member"]),
                    _ => ("LLEN", "LRANGE", &["value"]),
                };
                let len: i64 = self.query(redis::cmd(len_cmd).arg(key)).await?;
                if let Some((mut at, last)) = bounds(*start, *stop, len) {
                    while at <= last {
                        let to = (at + i64::try_from(PAGE).unwrap_or(1_000) - 1).min(last);
                        let mut cmd = redis::cmd(range_cmd);
                        cmd.arg(key).arg(at).arg(to);
                        if matches!(p, Paged::Zset { scores: true, .. }) {
                            cmd.arg("WITHSCORES");
                        }
                        let reply: R = self.query(&cmd).await?;
                        let items = match &reply {
                            R::Array(i) | R::Set(i) => i.as_slice(),
                            _ => &[],
                        };
                        if items.is_empty() {
                            break;
                        }
                        let (_, rows) = value::list_rows(items);
                        if !self.page(names, rows, &mut first).await {
                            return Ok(());
                        }
                        at = to + 1;
                    }
                }
                if first && !self.page(names, Vec::new(), &mut first).await {
                    return Ok(());
                }
            }
            Paged::Hash { key } | Paged::Set { key } => {
                let (scan, names): (&str, &[&str]) = match p {
                    Paged::Hash { .. } => ("HSCAN", &["field", "value"]),
                    _ => ("SSCAN", &["member"]),
                };
                let mut cursor = "0".to_owned();
                loop {
                    let (next, items): (String, Vec<R>) = self
                        .query(
                            redis::cmd(scan)
                                .arg(key)
                                .arg(&cursor)
                                .arg("COUNT")
                                .arg(PAGE),
                        )
                        .await?;
                    let rows: Vec<Vec<treasury_core::Value>> = if names.len() == 2 {
                        items
                            .chunks(2)
                            .map(|kv| kv.iter().map(value::cell).collect())
                            .collect()
                    } else {
                        items.iter().map(|m| vec![value::cell(m)]).collect()
                    };
                    if !self.page(names, rows, &mut first).await {
                        return Ok(());
                    }
                    if next == "0" {
                        break;
                    }
                    cursor = next;
                }
            }
        }
        let _ = self
            .send(Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            }))
            .await;
        Ok(())
    }
}

#[async_trait]
impl Session for RedisSession {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            language: QueryLanguage::Redis,
            introspect: true,
            ddl: false,
            explain: None,
            cancel: CancelMethod::KillFromSecondConnection,
            health: false,
            edit: false,
            transactions: false,
            read_only: ReadOnlyEnforcement::Client,
            multiple_results: false,
        }
    }

    fn analyzer(&self) -> &dyn Analyzer {
        &self.analyzer
    }

    async fn ping(&self) -> Result<Duration> {
        self.alive()?;
        let started = Instant::now();
        let mut conn = self.conn().await;
        let info: String = self
            .meta(&mut conn, redis::cmd("INFO").arg("server"))
            .await?;
        // A new run id is a restarted server: keys may be gone, and blocked clients were let go.
        if self.run_id.is_some() && info_field(&info, "run_id") != self.run_id.as_deref() {
            return Err(Error::new(
                ErrorKind::Network {
                    host: self.host.clone(),
                    port: self.port,
                    cause: treasury_core::error::NetworkCause::Reset,
                },
                "The server restarted since this connection was opened.",
            ));
        }
        Ok(started.elapsed())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo {
            product: match self.engine {
                Engine::Valkey => "Valkey".into(),
                _ => "Redis".into(),
            },
            version: self.version.clone(),
            details: vec![
                ("Database".to_owned(), self.db.to_string()),
                ("Mode".to_owned(), self.mode.to_owned()),
            ],
        })
    }

    async fn set_read_only(&self, _read_only: bool) -> Result<()> {
        // Redis has no read-only session; the guardrails enforce it from the command table.
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
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
}

#[async_trait]
impl Execute for RedisSession {
    async fn execute(&self, req: ExecRequest) -> Result<ExecStream> {
        self.alive()?;
        let pieces = lang::pieces(&req.text);
        let [piece] = pieces.as_slice() else {
            return Err(Error::unsupported(
                "This text holds more than one command. Treasury runs them one at a time.",
            ));
        };
        let argv = piece
            .words
            .clone()
            .map_err(|e| Error::new(ErrorKind::Syntax, e))?;
        let name = argv
            .first()
            .map(|n| n.to_ascii_lowercase())
            .unwrap_or_default();
        if lang::UNSUPPORTED.contains(&name.as_str()) {
            let why = self
                .analyzer
                .classify(&req.text)
                .first()
                .and_then(|c| c.reason.clone())
                .unwrap_or_default();
            return Err(Error::unsupported(why));
        }
        let writes = self
            .analyzer
            .classify(&req.text)
            .first()
            .is_some_and(|c| c.class != StatementClass::Read);
        if self.broken.swap(false, Ordering::SeqCst) {
            self.reopen().await?;
        }
        self.stopped.store(false, Ordering::SeqCst);
        self.timed_out.store(false, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(1);
        let run = Run {
            conn: self.conn().await,
            argv,
            writes,
            host: self.host.clone(),
            port: self.port,
            stopped: self.stopped.clone(),
            timed_out: self.timed_out.clone(),
            limit: req.timeout,
            tx,
        };
        let task = tokio::spawn(run.run());
        if let Some(limit) = req.timeout {
            let (stopped, flag) = (self.stopped.clone(), self.timed_out.clone());
            let this = self.side();
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                if !task.is_finished() {
                    flag.store(true, Ordering::SeqCst);
                    stopped.store(true, Ordering::SeqCst);
                    this.unblock_or_kill().await;
                }
            });
        }
        Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }
}

/// What stopping a command from another task needs, without the session itself.
struct SideCancel {
    client: Client,
    id: Arc<AtomicI64>,
    timeout: Duration,
    broken: Arc<AtomicBool>,
}

impl SideCancel {
    /// Unblocks the session's command when it is blocked (BLPOP and the like), and otherwise
    /// ends the session's connection, the only way to stop a long command in Redis.
    async fn unblock_or_kill(&self) {
        let options = AsyncConnectionConfig::new().set_connection_timeout(Some(self.timeout));
        let Ok(mut side) = self
            .client
            .get_multiplexed_async_connection_with_config(&options)
            .await
        else {
            return;
        };
        let id = self.id.load(Ordering::SeqCst);
        let unblocked: i64 = redis::cmd("CLIENT")
            .arg("UNBLOCK")
            .arg(id)
            .arg("ERROR")
            .query_async(&mut side)
            .await
            .unwrap_or(0);
        if unblocked == 0 {
            let _ = redis::cmd("CLIENT")
                .arg("KILL")
                .arg("ID")
                .arg(id)
                .query_async::<R>(&mut side)
                .await;
            self.broken.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Cancel for RedisSession {
    async fn cancel(&self) -> Result<()> {
        self.stopped.store(true, Ordering::SeqCst);
        self.side().unblock_or_kill().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        lang::words(s).unwrap()
    }

    #[test]
    fn large_replies_are_read_in_pages() {
        assert_eq!(
            paged(&argv("LRANGE big 0 -1")),
            Some(Paged::List {
                key: "big".into(),
                start: 0,
                stop: -1
            })
        );
        assert!(matches!(
            paged(&argv("zrange z 0 9 withscores")),
            Some(Paged::Zset { scores: true, .. })
        ));
        assert!(paged(&argv("ZRANGE z 0 9 BYSCORE")).is_none());
        assert!(paged(&argv("HGETALL h")).is_some());
        assert!(paged(&argv("GET k")).is_none());
    }

    #[test]
    fn positions_count_from_the_end_like_redis() {
        assert_eq!(bounds(0, -1, 10), Some((0, 9)));
        assert_eq!(bounds(-3, -1, 10), Some((7, 9)));
        assert_eq!(bounds(5, 100, 10), Some((5, 9)));
        assert_eq!(bounds(0, -1, 0), None);
        assert_eq!(bounds(8, 2, 10), None);
    }

    #[test]
    fn info_fields_are_read() {
        let info = "# Server\r\nredis_version:7.4.1\r\nrun_id:abc\r\n";
        assert_eq!(info_field(info, "redis_version"), Some("7.4.1"));
        assert_eq!(info_field(info, "run_id"), Some("abc"));
        assert_eq!(info_field(info, "missing"), None);
    }
}
