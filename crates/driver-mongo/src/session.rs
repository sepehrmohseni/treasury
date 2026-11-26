//! One MongoDB connection (a driver client, which pools its own sockets).

use crate::error;
use crate::lang::{self, MongoAnalyzer};
use crate::value;
use async_trait::async_trait;
use futures::{StreamExt, stream};
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use secrecy::ExposeSecret;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use treasury_core::analyze::Analyzer;
use treasury_core::config::{ConnectionConfig, Endpoint, SecretSlot, TlsMode};
use treasury_core::driver::{
    Cancel, CancelMethod, Capabilities, ConnectContext, Edit, Execute, Explain, ExplainSupport,
    Health, Introspect, QueryLanguage, ReadOnlyEnforcement, ServerInfo, Session,
};
use treasury_core::exec::{ExecEvent, ExecRequest, ExecStream};
use treasury_core::{Error, ErrorKind, Result};
use treasury_tunnel::TlsBridge;

/// Commands that answer with a cursor rather than one document.
const CURSOR_COMMANDS: &[&str] = &["find", "aggregate", "listCollections", "listIndexes"];

/// An open MongoDB session.
pub struct MongoSession {
    pub(crate) client: mongodb::Client,
    /// The database commands run against unless they name another with `"$db"`.
    pub(crate) database: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) metadata_timeout: Duration,
    analyzer: MongoAnalyzer,
    /// The server process seen at connect; a different one means it restarted.
    process: Option<Bson>,
    closed: AtomicBool,
    /// The comment tagging the command now running, used to find and kill it.
    current: Arc<std::sync::Mutex<Option<String>>>,
    timed_out: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    /// Holds the combined certificate and key file for the driver while the session lives.
    _tls_files: Option<tempfile::TempDir>,
    /// The TLS bridge the driver connects through, when Treasury makes the TLS.
    _bridge: Option<TlsBridge>,
}

/// A tag that identifies one command in `currentOp`.
fn temp_name() -> String {
    format!("treasury:{}", mongodb::bson::oid::ObjectId::new().to_hex())
}

/// Writes the client certificate and its key (still encrypted, when it is) into one private
/// file, the form the driver reads.
fn client_identity(
    config: &ConnectionConfig,
) -> Result<Option<(tempfile::TempDir, std::path::PathBuf)>> {
    let (Some(cert), Some(key)) = (&config.tls.client_cert, &config.tls.client_key) else {
        return Ok(None);
    };
    let read = |p: &std::path::Path| {
        std::fs::read_to_string(p).map_err(|e| {
            Error::config(
                Some("tls"),
                format!("{} could not be read: {e}.", p.display()),
            )
        })
    };
    let dir = tempfile::Builder::new()
        .prefix("treasury-tls-")
        .tempdir()
        .map_err(|e| Error::internal(format!("a private folder for TLS files: {e}")))?;
    let path = dir.path().join("identity.pem");
    let text = format!("{}\n{}\n", read(cert)?, read(key)?);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| Error::internal(format!("writing the TLS identity: {e}")))?;
        f.write_all(text.as_bytes())
            .map_err(|e| Error::internal(format!("writing the TLS identity: {e}")))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, text)
        .map_err(|e| Error::internal(format!("writing the TLS identity: {e}")))?;
    Ok(Some((dir, path)))
}

pub(crate) async fn connect(
    config: &ConnectionConfig,
    ctx: ConnectContext,
) -> Result<MongoSession> {
    let (host, port, hosts) = match &config.endpoint {
        Endpoint::Host { host, port } => (host.clone(), *port, vec![(host.clone(), *port)]),
        Endpoint::Hosts { hosts } if !hosts.is_empty() => {
            (hosts[0].0.clone(), hosts[0].1, hosts.clone())
        }
        _ => {
            return Err(Error::config(
                Some("host"),
                "MongoDB connections need a host and port.",
            ));
        }
    };
    // One server (or a tunnel to one): Treasury makes the TLS itself through a bridge, so the
    // certificate is checked against the configured name with the shared rules. Replica set
    // discovery dials every member by name, so there the driver's own TLS is used.
    let direct =
        ctx.via.is_some() || (hosts.len() == 1 && !config.options.contains_key("replicaSet"));
    let tls_on = config.tls.mode != TlsMode::Disable;
    let passphrase = ctx.secrets.secret(SecretSlot::KeyPassphrase)?;
    let mut bridge = None;
    if direct && tls_on {
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
        let rustls = treasury_tunnel::tls::client_config(&config.tls, passphrase.as_ref())?;
        let name = treasury_tunnel::tls::server_name(&config.tls, &host)?;
        match TlsBridge::probe(upstream, rustls.clone(), name.clone(), &host, port).await {
            Ok(()) => bridge = Some(TlsBridge::start(upstream, rustls, name).await?),
            // Prefer: a server that does not speak TLS is used without it.
            Err(e)
                if config.tls.mode == TlsMode::Prefer
                    && !matches!(e.kind, ErrorKind::Network { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    let mut opts = ClientOptions::default();
    opts.hosts = match (&bridge, ctx.via) {
        (Some(b), _) => vec![ServerAddress::Tcp {
            host: b.local_addr().ip().to_string(),
            port: Some(b.local_addr().port()),
        }],
        (None, Some(a)) => vec![ServerAddress::Tcp {
            host: a.ip().to_string(),
            port: Some(a.port()),
        }],
        (None, None) => hosts
            .iter()
            .map(|(h, p)| ServerAddress::Tcp {
                host: h.clone(),
                port: Some(*p),
            })
            .collect(),
    };
    opts.direct_connection = Some(direct);
    opts.repl_set_name = config.options.get("replicaSet").cloned();
    opts.app_name = Some("Treasury".into());
    opts.connect_timeout = Some(ctx.connect_timeout);
    opts.server_selection_timeout = Some(ctx.connect_timeout);
    if let Some(user) = &config.user {
        let password = ctx.secrets.secret(SecretSlot::Password)?;
        opts.credential = Some(
            Credential::builder()
                .username(user.clone())
                .password(password.map(|p| p.expose_secret().to_owned()))
                .source(
                    config
                        .options
                        .get("authSource")
                        .cloned()
                        .or_else(|| Some("admin".to_owned())),
                )
                .build(),
        );
    }
    let native_tls = tls_on && !direct;
    let identity = if native_tls {
        client_identity(config)?
    } else {
        None
    };
    if native_tls {
        let mut t = TlsOptions::default();
        t.ca_file_path = config.tls.ca_file.clone();
        t.allow_invalid_certificates = Some(config.tls.accept_invalid);
        if let Some((_, path)) = &identity {
            t.cert_key_file_path = Some(path.clone());
            // The key stays encrypted on disk; the driver unlocks it with the passphrase.
            t.tls_certificate_key_file_password = passphrase
                .as_ref()
                .map(|p| p.expose_secret().as_bytes().to_vec());
        }
        opts.tls = Some(Tls::Enabled(t));
    } else {
        opts.tls = Some(Tls::Disabled);
    }
    let client = mongodb::Client::with_options(opts).map_err(|e| error::map(&e, &host, port))?;
    let hello = client
        .database("admin")
        .run_command(doc! {"hello": 1})
        .await
        .map_err(|e| error::map(&e, &host, port))?;
    let process = hello
        .get_document("topologyVersion")
        .ok()
        .and_then(|t| t.get("processId").cloned());
    Ok(MongoSession {
        client,
        database: config.database.clone().unwrap_or_else(|| "test".into()),
        host,
        port,
        metadata_timeout: ctx.metadata_timeout,
        analyzer: MongoAnalyzer,
        process,
        closed: AtomicBool::new(false),
        current: Arc::new(std::sync::Mutex::new(None)),
        timed_out: Arc::new(AtomicBool::new(false)),
        stopped: Arc::new(AtomicBool::new(false)),
        _tls_files: identity.map(|(d, _)| d),
        _bridge: bridge,
    })
}

/// Finds the running command tagged `tag` and kills it.
async fn kill(client: &mongodb::Client, tag: &str) {
    let admin = client.database("admin");
    let Ok(ops) = admin
        .run_command(doc! {"currentOp": 1, "$ownOps": true, "command.comment": tag})
        .await
    else {
        return;
    };
    if let Ok(list) = ops.get_array("inprog") {
        for op in list.iter().filter_map(Bson::as_document) {
            if let Some(id) = op.get("opid") {
                let _ = admin
                    .run_command(doc! {"killOp": 1, "op": id.clone()})
                    .await;
            }
        }
    }
}

impl MongoSession {
    pub(crate) fn map(&self, e: &mongodb::error::Error) -> Error {
        error::map(e, &self.host, self.port)
    }

    pub(crate) fn alive(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::network(
                &self.host,
                self.port,
                &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
            ));
        }
        Ok(())
    }

    /// Runs a command for metadata under the metadata time limit.
    pub(crate) async fn meta(&self, db: &str, command: Document) -> Result<Document> {
        self.alive()?;
        let mut command = command;
        let ms = i64::try_from(self.metadata_timeout.as_millis()).unwrap_or(i64::MAX);
        if !matches!(
            command.keys().next().map(String::as_str),
            Some("hello" | "ping" | "buildInfo" | "connectionStatus")
        ) {
            command.insert("maxTimeMS", ms);
        }
        tokio::time::timeout(
            self.metadata_timeout + Duration::from_secs(2),
            self.client.database(db).run_command(command),
        )
        .await
        .map_err(|_| Error::timeout(self.metadata_timeout))?
        .map_err(|e| self.map(&e))
    }

    /// The documents a cursor command returns, at most `limit`.
    pub(crate) async fn meta_cursor(
        &self,
        db: &str,
        command: Document,
        limit: usize,
    ) -> Result<Vec<Document>> {
        self.alive()?;
        let work = async {
            let mut cursor = self.client.database(db).run_cursor_command(command).await?;
            let mut out = Vec::new();
            while out.len() < limit {
                match cursor.next().await {
                    Some(d) => out.push(d?),
                    None => break,
                }
            }
            Ok::<_, mongodb::error::Error>(out)
        };
        tokio::time::timeout(self.metadata_timeout, work)
            .await
            .map_err(|_| Error::timeout(self.metadata_timeout))?
            .map_err(|e| self.map(&e))
    }
}

/// What the task streaming one command needs.
struct Run {
    client: mongodb::Client,
    db: String,
    command: Document,
    cursor: bool,
    batch: usize,
    host: String,
    port: u16,
    tag: String,
    stopped: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    limit: Option<Duration>,
    tx: mpsc::Sender<Result<ExecEvent>>,
}

impl Run {
    fn map(&self, e: &mongodb::error::Error) -> Error {
        let err = error::map(e, &self.host, self.port);
        if matches!(err.kind, ErrorKind::Cancelled | ErrorKind::Timeout { .. }) {
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

    async fn run(self) {
        let outcome = if self.cursor {
            self.stream().await
        } else {
            self.single().await
        };
        let outcome = match outcome {
            Ok(()) if self.stopped.load(Ordering::SeqCst) => Err(self.stop_error()),
            other => other,
        };
        if let Err(e) = outcome {
            let _ = self.tx.send(Err(e)).await;
        }
    }

    async fn stream(&self) -> Result<()> {
        let db = self.client.database(&self.db);
        // Every batch, not only the first, stays at the requested size; the server's default is
        // up to 16 MB per batch.
        let mut cursor = db
            .run_cursor_command(self.command.clone())
            .batch_size(u32::try_from(self.batch.max(2_000)).unwrap_or(2_000))
            .await
            .map_err(|e| self.map(&e))?
            .with_type::<mongodb::bson::RawDocumentBuf>();
        // The first batch decides the columns.
        let mut first = Vec::new();
        while first.len() < self.batch {
            match cursor.next().await {
                Some(d) => first.push(d.map_err(|e| self.map(&e))?),
                None => break,
            }
        }
        let names = value::raw_columns(&first);
        let rows: Vec<Vec<treasury_core::Value>> =
            first.iter().map(|d| value::raw_row(&names, d)).collect();
        if !self
            .send(Ok(ExecEvent::ResultStart(value::describe(&names, &rows))))
            .await
        {
            return Ok(());
        }
        if !rows.is_empty() && !self.send(Ok(ExecEvent::Rows(rows))).await {
            return Ok(());
        }
        let mut batch = Vec::with_capacity(self.batch);
        while let Some(d) = cursor.next().await {
            batch.push(value::raw_row(&names, &d.map_err(|e| self.map(&e))?));
            if batch.len() >= self.batch
                && !self
                    .send(Ok(ExecEvent::Rows(std::mem::take(&mut batch))))
                    .await
            {
                // Dropping the cursor closes it on the server.
                return Ok(());
            }
        }
        if !batch.is_empty() && !self.send(Ok(ExecEvent::Rows(batch))).await {
            return Ok(());
        }
        let _ = self
            .send(Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated: false,
            }))
            .await;
        Ok(())
    }

    async fn single(&self) -> Result<()> {
        let reply = self
            .client
            .database(&self.db)
            .run_command(self.command.clone())
            .await
            .map_err(|e| self.map(&e))?;
        let name = self.command.keys().next().map(String::as_str).unwrap_or("");
        if matches!(name, "insert" | "update" | "delete") {
            let n = reply
                .get("n")
                .and_then(|v| match v {
                    Bson::Int32(n) => u64::try_from(*n).ok(),
                    Bson::Int64(n) => u64::try_from(*n).ok(),
                    _ => None,
                })
                .unwrap_or(0);
            if let Ok(errors) = reply.get_array("writeErrors")
                && let Some(first) = errors.first().and_then(Bson::as_document)
            {
                return Err(Error::new(
                    ErrorKind::Server,
                    first
                        .get_str("errmsg")
                        .unwrap_or("The write failed.")
                        .to_owned(),
                ));
            }
            let _ = self
                .send(Ok(ExecEvent::ResultEnd {
                    affected: Some(n),
                    truncated: false,
                }))
                .await;
            return Ok(());
        }
        let names = value::columns(std::slice::from_ref(&reply));
        let row = value::row(&names, reply);
        let cols = value::describe(&names, std::slice::from_ref(&row));
        if self.send(Ok(ExecEvent::ResultStart(cols))).await
            && self.send(Ok(ExecEvent::Rows(vec![row]))).await
        {
            let _ = self
                .send(Ok(ExecEvent::ResultEnd {
                    affected: None,
                    truncated: false,
                }))
                .await;
        }
        Ok(())
    }
}

#[async_trait]
impl Session for MongoSession {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            language: QueryLanguage::Mongo,
            introspect: true,
            ddl: false,
            explain: Some(ExplainSupport { analyze: true }),
            cancel: CancelMethod::KillFromSecondConnection,
            health: true,
            edit: true,
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
        let hello = self.meta("admin", doc! {"hello": 1}).await?;
        let now = hello
            .get_document("topologyVersion")
            .ok()
            .and_then(|t| t.get("processId").cloned());
        // The driver reconnects by itself; a new server process still means cursors and
        // sessions from before are gone, which the user should know.
        if self.process.is_some() && now != self.process {
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
        let build = self.meta("admin", doc! {"buildInfo": 1}).await?;
        let hello = self.meta("admin", doc! {"hello": 1}).await?;
        let mut details = vec![("Database".to_owned(), self.database.clone())];
        if let Ok(set) = hello.get_str("setName") {
            details.push(("Replica set".to_owned(), set.to_owned()));
        }
        details.push((
            "Role".to_owned(),
            if hello.get_bool("isWritablePrimary").unwrap_or(false) {
                "primary".to_owned()
            } else {
                "secondary".to_owned()
            },
        ));
        Ok(ServerInfo {
            product: "MongoDB".into(),
            version: build.get_str("version").unwrap_or("").to_owned(),
            details,
        })
    }

    async fn set_read_only(&self, _read_only: bool) -> Result<()> {
        // MongoDB has no read-only session; the guardrails enforce it.
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        let tag = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(tag) = tag {
            kill(&self.client, &tag).await;
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

/// The command in `text`, the database it runs against, and whether it answers with a cursor.
pub(crate) fn parse_command(text: &str, default_db: &str) -> Result<(String, Document)> {
    let pieces = lang::pieces(text);
    let [piece] = pieces.as_slice() else {
        return Err(Error::unsupported(
            "This text holds more than one command. Treasury runs them one at a time.",
        ));
    };
    let json = piece
        .doc
        .clone()
        .map_err(|e| Error::new(ErrorKind::Syntax, e))?;
    let mut command = value::document(json).map_err(|e| Error::new(ErrorKind::Syntax, e))?;
    let db = match command.remove("$db") {
        Some(Bson::String(db)) => db,
        _ => default_db.to_owned(),
    };
    Ok((db, command))
}

#[async_trait]
impl Execute for MongoSession {
    async fn execute(&self, req: ExecRequest) -> Result<ExecStream> {
        self.alive()?;
        let (db, mut command) = parse_command(&req.text, &self.database)?;
        let name = command.keys().next().cloned().unwrap_or_default();
        let cursor = CURSOR_COMMANDS.contains(&name.as_str());
        let batch = usize::try_from(req.fetch_rows.max(1)).unwrap_or(500);
        if name == "aggregate" && !command.contains_key("cursor") {
            command.insert(
                "cursor",
                doc! {"batchSize": i64::try_from(batch).unwrap_or(500)},
            );
        }
        if name == "find" && !command.contains_key("batchSize") {
            command.insert("batchSize", i64::try_from(batch).unwrap_or(500));
        }
        let tag = temp_name();
        if !command.contains_key("comment") {
            command.insert("comment", tag.clone());
        }
        if let Some(limit) = req.timeout {
            command.insert(
                "maxTimeMS",
                i64::try_from(limit.as_millis()).unwrap_or(i64::MAX),
            );
        }
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(tag.clone());
        self.stopped.store(false, Ordering::SeqCst);
        self.timed_out.store(false, Ordering::SeqCst);
        let timer = req.timeout.map(|limit| {
            let (client, tag) = (self.client.clone(), tag.clone());
            let (stopped, flag) = (self.stopped.clone(), self.timed_out.clone());
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                stopped.store(true, Ordering::SeqCst);
                kill(&client, &tag).await;
            })
            .abort_handle()
        });
        let (tx, rx) = mpsc::channel(1);
        let run = Run {
            client: self.client.clone(),
            db,
            command,
            cursor,
            batch,
            host: self.host.clone(),
            port: self.port,
            tag,
            stopped: self.stopped.clone(),
            timed_out: self.timed_out.clone(),
            limit: req.timeout,
            tx,
        };
        let current = self.current.clone();
        tokio::spawn(async move {
            let tag = run.tag.clone();
            run.run().await;
            if let Some(t) = timer {
                t.abort();
            }
            let mut cur = current.lock().unwrap_or_else(PoisonError::into_inner);
            if cur.as_deref() == Some(tag.as_str()) {
                *cur = None;
            }
        });
        Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }
}

#[async_trait]
impl Cancel for MongoSession {
    async fn cancel(&self) -> Result<()> {
        let tag = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(tag) = tag {
            self.stopped.store(true, Ordering::SeqCst);
            kill(&self.client, &tag).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_name_their_database() {
        let (db, c) = parse_command(r#"{"find": "o", "$db": "shop"}"#, "test").unwrap();
        assert_eq!(db, "shop");
        assert!(!c.contains_key("$db"));
        let (db, _) = parse_command(r#"{"ping": 1}"#, "test").unwrap();
        assert_eq!(db, "test");
        assert_eq!(
            parse_command(r#"{"ping": 1} {"ping": 1}"#, "t")
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            parse_command("{oops", "t").unwrap_err().kind,
            ErrorKind::Syntax
        );
    }

    #[test]
    fn tags_differ() {
        assert_ne!(temp_name(), temp_name());
        assert!(temp_name().starts_with("treasury:"));
    }
}
