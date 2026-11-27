//! The engine room behind the desktop app and the command line: open connections (through SSH
//! tunnels when configured), the guardrail flow before a statement runs, running statements into
//! result cursors, and the write unlock. The app's commands call this and only display results.
//!
//! A statement runs in two steps. [`Runtime::prepare`] reviews it with the guardrails and does
//! what they ask that needs the server (counting the rows a change would touch); the user sees
//! the outcome. [`Runtime::run`] reviews it again and refuses unless the confirmations the
//! review requires are present, so the UI can never skip a guardrail.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use treasury_core::analyze::{CountPlan, LimitPlan, StatementClass};
use treasury_core::cache::PageStore;
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine, Environment};
use treasury_core::cursor::{Execution, ExecutionOptions, ExecutionStatus, Window};
use treasury_core::driver::{ConnectContext, Driver, ServerInfo, Session};
use treasury_core::exec::{ExecEvent, ExecRequest, ExecStream};
use treasury_core::guard::{
    self, BlockReason, Confirmation, DEFAULT_COUNT_THRESHOLD, Decision, NameReason, Policy,
    UNLOCK_DURATION,
};
use treasury_core::value::Value;
use treasury_core::{Error, ErrorKind, Result};
use treasury_store::{HistoryEntry, RunStatus, Store};
use treasury_tunnel::{HostKeyPrompt, KnownHosts, Tunnel, TunnelParams};
use treasury_vault::{ConnectionSecrets, SecretStore};

/// The default row limit for plain SELECTs.
pub const DEFAULT_ROW_LIMIT: u64 = 1_000;
/// How long a prepared confirmation stays valid.
const PREPARED_FOR: Duration = Duration::from_secs(120);
/// The page cache budget for each result.
const CACHE_BUDGET: usize = 64 * 1024 * 1024;

/// An open connection: one session for the user's statements, one for the tree and previews, so
/// browsing never waits behind a long query.
pub struct Live {
    /// The saved settings it was opened with.
    pub config: ConnectionConfig,
    /// Runs the user's statements.
    pub session: Arc<dyn Session>,
    /// Introspection, counts and other metadata.
    pub meta: Arc<dyn Session>,
    /// Facts shown in the status bar.
    pub info: ServerInfo,
    unlocked_until: Mutex<Option<Instant>>,
    _tunnel: Option<Tunnel>,
}

impl Live {
    /// Whether the connection starts read-only: configured so, or production.
    fn read_only_by_default(&self) -> bool {
        self.config.read_only || self.config.environment == Environment::Production
    }

    /// When "Unlock writes" ends, if it is on.
    pub fn unlocked_until(&self) -> Option<Instant> {
        let mut u = self
            .unlocked_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if u.is_some_and(|t| t <= Instant::now()) {
            *u = None;
        }
        *u
    }

    /// Whether writes are refused right now.
    pub fn read_only(&self) -> bool {
        self.read_only_by_default() && self.unlocked_until().is_none()
    }

    fn policy(&self) -> Policy {
        Policy {
            connection_name: self.config.name.clone(),
            environment: self.config.environment,
            read_only: self.read_only(),
            enforcement: self.session.capabilities().read_only,
            count_threshold: DEFAULT_COUNT_THRESHOLD,
        }
    }
}

pub use treasury_ipc::Prepared;

struct Ticket {
    connection: ConnectionId,
    statement: String,
    count: Option<u64>,
    name_required: bool,
    at: Instant,
}

/// A statement started by [`Runtime::run`].
pub struct Running {
    /// The connection it runs on.
    pub connection: ConnectionId,
    /// The result cursor.
    pub execution: Arc<Execution>,
    /// The text sent to the server (with the row limit, when one was added).
    pub text: String,
    started: Instant,
    /// The history entry written when the statement started, until it is completed.
    history: Mutex<Option<i64>>,
    /// The user cancelled it.
    cancelled: AtomicBool,
    edit_target: Mutex<Option<edit::Target>>,
}

/// The runtime: connections, guardrails and running statements.
pub struct Runtime {
    store: Arc<Store>,
    secrets: Arc<dyn SecretStore>,
    drivers: HashMap<Engine, Arc<dyn Driver>>,
    known_hosts: KnownHosts,
    prompt: Arc<dyn HostKeyPrompt>,
    spill_dir: PathBuf,
    live: tokio::sync::Mutex<HashMap<ConnectionId, Arc<Live>>>,
    tickets: Mutex<HashMap<u64, Ticket>>,
    running: Mutex<HashMap<u64, Arc<Running>>>,
    next: AtomicU64,
}

fn guard_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Guard, message)
}

fn block_text(reason: BlockReason) -> &'static str {
    match reason {
        BlockReason::ReadOnly => {
            "The connection is read-only, and this statement could write. Unlock writes to run it."
        }
        BlockReason::ReadOnlyEscape => {
            "This statement would switch the connection's read-only mode off."
        }
        BlockReason::ProductionForbidden => "This statement is not allowed on production.",
    }
}

fn name_text(reason: &NameReason) -> String {
    match reason {
        NameReason::Destructive { .. } => {
            "It drops, truncates or alters a table on production.".into()
        }
        NameReason::Unfiltered => "It has no WHERE clause, so it changes every row.".into(),
        NameReason::ManyRows { count } => format!("It changes {count} rows."),
        NameReason::NoPreview { detail } => detail.clone(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl Runtime {
    /// A runtime over the app's store and secret store. `spill_dir` holds encrypted page
    /// spills of large results.
    pub fn new(
        store: Arc<Store>,
        secrets: Arc<dyn SecretStore>,
        drivers: Vec<Arc<dyn Driver>>,
        known_hosts: KnownHosts,
        prompt: Arc<dyn HostKeyPrompt>,
        spill_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            secrets,
            drivers: drivers.into_iter().map(|d| (d.engine(), d)).collect(),
            known_hosts,
            prompt,
            spill_dir,
            live: tokio::sync::Mutex::new(HashMap::new()),
            tickets: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    /// The local database.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The secrets of one connection.
    pub fn secrets_of(&self, id: &ConnectionId) -> ConnectionSecrets {
        ConnectionSecrets::new(self.secrets.clone(), id.clone())
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn row_limit(&self) -> u64 {
        self.store
            .setting::<u64>("row_limit")
            .ok()
            .flatten()
            .unwrap_or(DEFAULT_ROW_LIMIT)
    }

    async fn open_session(
        &self,
        driver: &Arc<dyn Driver>,
        config: &ConnectionConfig,
        via: Option<std::net::SocketAddr>,
        read_only: bool,
    ) -> Result<Arc<dyn Session>> {
        let ctx = ConnectContext {
            secrets: Arc::new(self.secrets_of(&config.id)),
            via,
            connect_timeout: Duration::from_secs(20),
            metadata_timeout: Duration::from_secs(30),
            read_only,
        };
        driver.connect(config, ctx).await
    }

    /// Opens `config` (replacing an open connection with the same id).
    pub async fn connect(&self, config: &ConnectionConfig) -> Result<Arc<Live>> {
        let driver = self.drivers.get(&config.engine).cloned().ok_or_else(|| {
            Error::unsupported(format!(
                "{} is not available in this version.",
                config.engine.display_name()
            ))
        })?;
        let tunnel = match &config.ssh {
            Some(ssh) => {
                let (host, port) = match &config.endpoint {
                    treasury_core::config::Endpoint::Host { host, port } => (host.clone(), *port),
                    _ => {
                        return Err(Error::config(
                            Some("ssh"),
                            "An SSH tunnel needs a single host and port.",
                        ));
                    }
                };
                let secrets = self.secrets_of(&config.id);
                Some(
                    Tunnel::open(TunnelParams {
                        ssh,
                        target: (&host, port),
                        secrets: &secrets,
                        known_hosts: &self.known_hosts,
                        prompt: self.prompt.clone(),
                        timeout: Duration::from_secs(20),
                    })
                    .await?,
                )
            }
            None => None,
        };
        let via = tunnel.as_ref().map(Tunnel::local_addr);
        let read_only = config.read_only || config.environment == Environment::Production;
        let session = self.open_session(&driver, config, via, read_only).await?;
        let meta = self.open_session(&driver, config, via, true).await?;
        let info = session.server_info().await?;
        let live = Arc::new(Live {
            config: config.clone(),
            session,
            meta,
            info,
            unlocked_until: Mutex::new(None),
            _tunnel: tunnel,
        });
        if let Some(old) = self
            .live
            .lock()
            .await
            .insert(config.id.clone(), live.clone())
        {
            let _ = old.session.close().await;
            let _ = old.meta.close().await;
        }
        Ok(live)
    }

    /// The open connection `id`.
    pub async fn live(&self, id: &ConnectionId) -> Result<Arc<Live>> {
        self.live.lock().await.get(id).cloned().ok_or_else(|| {
            Error::new(
                ErrorKind::Config { field: None },
                "The connection is not open.",
            )
        })
    }

    /// Closes connection `id`.
    pub async fn disconnect(&self, id: &ConnectionId) {
        if let Some(live) = self.live.lock().await.remove(id) {
            let _ = live.session.close().await;
            let _ = live.meta.close().await;
        }
    }

    /// Allows writes on `id` for 15 minutes.
    pub async fn unlock_writes(&self, id: &ConnectionId) -> Result<Instant> {
        let live = self.live(id).await?;
        live.session.set_read_only(false).await?;
        let until = Instant::now() + UNLOCK_DURATION;
        *live
            .unlocked_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(until);
        Ok(until)
    }

    /// Ends an unlock early.
    pub async fn lock_writes(&self, id: &ConnectionId) -> Result<()> {
        let live = self.live(id).await?;
        *live
            .unlocked_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        if live.read_only_by_default() {
            live.session.set_read_only(true).await?;
        }
        Ok(())
    }

    /// Puts the server's read-only mode back when an unlock has run out.
    async fn relock_if_expired(&self, live: &Live) -> Result<()> {
        if live.read_only() {
            live.session.set_read_only(true).await?;
        }
        Ok(())
    }

    async fn count(&self, live: &Live, query: &str) -> Result<u64> {
        use futures::StreamExt;
        let exec = live
            .meta
            .execute()
            .ok_or_else(|| Error::unsupported("This connection cannot run statements."))?;
        let mut stream = exec
            .execute(ExecRequest::new(query).with_timeout(Duration::from_secs(30)))
            .await?;
        while let Some(event) = stream.next().await {
            if let ExecEvent::Rows(rows) = event? {
                return Ok(match rows.first().and_then(|r| r.first()) {
                    Some(Value::I64(n)) => u64::try_from(*n).unwrap_or(0),
                    Some(Value::U64(n)) => *n,
                    Some(Value::Decimal(d)) => d.parse().unwrap_or(0),
                    Some(Value::F64(f)) => *f as u64,
                    _ => 0,
                });
            }
        }
        // An aggregate count over no rows returns no row at all.
        Ok(0)
    }

    /// Reviews one statement with the guardrails and does the work they require before it may
    /// run: counting the rows an UPDATE or DELETE changes.
    pub async fn prepare(&self, id: &ConnectionId, statement: &str) -> Result<Prepared> {
        let live = self.live(id).await?;
        self.relock_if_expired(&live).await?;
        let policy = live.policy();
        let reviewed = guard::review(&policy, live.session.analyzer(), statement);
        let [one] = reviewed.as_slice() else {
            return Err(guard_error(
                "Select one statement to run, or run the script statement by statement.",
            ));
        };
        let mut prepared = Prepared {
            token: self.id(),
            class: one.statement.class,
            summary: one.statement.summary.clone(),
            refused: None,
            count: None,
            name_required: None,
        };
        match &one.decision {
            Decision::Allow => {}
            Decision::Block { reason } => prepared.refused = Some(block_text(*reason).into()),
            Decision::Confirm { steps } => {
                for step in steps {
                    match step {
                        Confirmation::CountRows { query } => {
                            let n = self.count(&live, query).await?;
                            prepared.count = Some(n);
                            if let Some(Confirmation::TypeName { reason }) =
                                guard::after_count(&policy, n)
                            {
                                prepared.name_required = Some(name_text(&reason));
                            }
                        }
                        Confirmation::TypeName { reason } => {
                            prepared.name_required = Some(name_text(reason));
                        }
                    }
                }
            }
        }
        self.tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                prepared.token,
                Ticket {
                    connection: id.clone(),
                    statement: statement.to_owned(),
                    count: prepared.count,
                    name_required: prepared.name_required.is_some(),
                    at: Instant::now(),
                },
            );
        Ok(prepared)
    }

    /// Runs a statement [`Runtime::prepare`] allowed. The review is repeated here; anything it
    /// requires must have been done: the same statement prepared moments ago, and the connection
    /// name typed when asked for. `limit` false runs a plain SELECT without the row limit.
    pub async fn run(
        &self,
        id: &ConnectionId,
        statement: &str,
        token: u64,
        typed_name: Option<&str>,
        limit: bool,
    ) -> Result<u64> {
        let live = self.live(id).await?;
        self.relock_if_expired(&live).await?;
        let ticket = self
            .tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&token)
            .ok_or_else(|| guard_error("Review the statement again before running it."))?;
        if ticket.connection != *id
            || ticket.statement != statement
            || ticket.at.elapsed() > PREPARED_FOR
        {
            return Err(guard_error(
                "The statement changed or its review is too old. Review it again.",
            ));
        }
        let policy = live.policy();
        let reviewed = guard::review(&policy, live.session.analyzer(), statement);
        let [one] = reviewed.as_slice() else {
            return Err(guard_error("Select one statement to run."));
        };
        match &one.decision {
            Decision::Allow => {}
            Decision::Block { reason } => return Err(guard_error(block_text(*reason))),
            Decision::Confirm { steps } => {
                let wants_count = steps
                    .iter()
                    .any(|s| matches!(s, Confirmation::CountRows { .. }));
                let wants_name = steps
                    .iter()
                    .any(|s| matches!(s, Confirmation::TypeName { .. }))
                    || ticket.name_required;
                if wants_count && ticket.count.is_none() {
                    return Err(guard_error("Count the rows it changes first."));
                }
                if wants_name && !typed_name.is_some_and(|t| guard::name_matches(&policy, t)) {
                    return Err(guard_error(
                        "Type the connection name exactly to run this statement.",
                    ));
                }
            }
        }
        let text = match (
            limit,
            live.session.analyzer().limit(statement, self.row_limit()),
        ) {
            (true, LimitPlan::Rewritten { text, .. }) => text,
            _ => statement.to_owned(),
        };
        let exec = live
            .session
            .execute()
            .ok_or_else(|| Error::unsupported("This connection cannot run statements."))?;
        let history = self.start_history(&live.config, statement);
        let began = Instant::now();
        let started = match exec.execute(ExecRequest::new(text.clone())).await {
            Ok(stream) => {
                PageStore::new(CACHE_BUDGET, self.spill_dir.clone()).map(|store| (stream, store))
            }
            Err(e) => Err(e),
        };
        let (stream, store) = match started {
            Ok(ok) => ok,
            Err(e) => {
                if let Some(entry) = history {
                    let took = i64::try_from(began.elapsed().as_millis()).unwrap_or(i64::MAX);
                    let _ = self
                        .store
                        .finish_history(entry, took, None, RunStatus::Failed);
                }
                return Err(e);
            }
        };
        let stream = only_writes_count(stream, one.statement.class);
        let max_rows = match (limit, live.session.analyzer().limit(statement, 1)) {
            (true, LimitPlan::StopFetching) => Some(self.row_limit()),
            _ => None,
        };
        let execution = Arc::new(Execution::new(
            stream,
            store,
            ExecutionOptions {
                page_rows: 1_000,
                max_rows,
            },
        ));
        let rid = self.id();
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                rid,
                Arc::new(Running {
                    connection: id.clone(),
                    execution,
                    text,
                    started: began,
                    history: Mutex::new(history),
                    cancelled: AtomicBool::new(false),
                    edit_target: Mutex::new(None),
                }),
            );
        Ok(rid)
    }

    /// Writes the history entry of `statement` as it starts, if the connection keeps history
    /// (its own setting, or else on except for production) and Settings has not turned it off.
    /// Writing it now means it is there even if Treasury quits before the statement ends.
    fn start_history(&self, config: &ConnectionConfig, statement: &str) -> Option<i64> {
        let keeps = config
            .history
            .unwrap_or(config.environment != Environment::Production);
        if !keeps || self.store.setting::<bool>("history").ok().flatten() == Some(false) {
            return None;
        }
        self.store
            .add_history(&HistoryEntry {
                id: 0,
                connection: config.id.clone(),
                text: statement.to_owned(),
                executed_at: now_ms(),
                duration_ms: 0,
                rows: None,
                status: RunStatus::Unfinished,
            })
            .ok()
    }

    fn running(&self, rid: u64) -> Result<Arc<Running>> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&rid)
            .cloned()
            .ok_or_else(|| Error::internal("that result is closed"))
    }

    /// Rows `start..start + count` of result set `set`.
    pub async fn window(&self, rid: u64, set: usize, start: u64, count: usize) -> Result<Window> {
        let r = self.running(rid)?;
        let w = r.execution.window(set, start, count).await;
        self.record(&r, false);
        w
    }

    /// How far a statement has got.
    pub fn status(&self, rid: u64) -> Result<ExecutionStatus> {
        let r = self.running(rid)?;
        self.record(&r, false);
        Ok(r.execution.status())
    }

    /// The result cursor, for exports.
    pub fn execution(&self, rid: u64) -> Result<Arc<Execution>> {
        Ok(self.running(rid)?.execution.clone())
    }

    /// Stops a running statement on the server.
    pub async fn cancel(&self, rid: u64) -> Result<()> {
        let r = self.running(rid)?;
        if let Ok(live) = self.live(&r.connection).await
            && let Some(c) = live.session.cancel()
        {
            c.cancel().await?;
        }
        r.cancelled.store(true, Ordering::Relaxed);
        r.execution.cancel();
        Ok(())
    }

    /// Forgets a result; its pages are freed and its spill file removed.
    pub fn close(&self, rid: u64) {
        if let Some(r) = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&rid)
        {
            r.execution.cancel();
            self.record(&r, true);
        }
    }

    /// Forgets every result, completing their history entries; for when Treasury quits.
    pub fn close_all(&self) {
        let all: Vec<Arc<Running>> = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, r)| r)
            .collect();
        for r in all {
            r.execution.cancel();
            self.record(&r, true);
        }
    }

    /// Completes the statement's history entry once it has finished, or when its result is
    /// closed: results are read only as far as they are shown, so a result can be closed before
    /// its stream has ended.
    fn record(&self, r: &Running, closing: bool) {
        let status = r.execution.status();
        if !status.finished && !closing {
            return;
        }
        let Some(id) = r
            .history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let rows: Option<i64> = status
            .sets
            .iter()
            .map(|s| s.affected.unwrap_or(s.rows))
            .reduce(|a, b| a + b)
            .and_then(|n| i64::try_from(n).ok());
        let outcome = match &status.error {
            Some(e) if e.kind == ErrorKind::Cancelled => RunStatus::Cancelled,
            Some(_) => RunStatus::Failed,
            None if r.cancelled.load(Ordering::Relaxed) => RunStatus::Cancelled,
            None => RunStatus::Ok,
        };
        let ended = r.execution.last_event().unwrap_or_else(Instant::now);
        let took = i64::try_from(ended.saturating_duration_since(r.started).as_millis())
            .unwrap_or(i64::MAX);
        let _ = self.store.finish_history(id, took, rows, outcome);
    }

    /// The count query for an UPDATE or DELETE, for showing before it runs.
    pub async fn count_plan(&self, id: &ConnectionId, statement: &str) -> Result<CountPlan> {
        Ok(self.live(id).await?.session.analyzer().count(statement))
    }
}

mod edit;

#[cfg(test)]
mod tests;

/// Drivers report a count of 0 for statements that change no rows by nature (`CREATE TABLE`,
/// `SET`), the same as for an `UPDATE` that matched nothing. Only statements that may write rows
/// keep their count, so the others read as done rather than as "0 rows changed".
fn only_writes_count(stream: ExecStream, class: StatementClass) -> ExecStream {
    use futures::StreamExt;
    if matches!(class, StatementClass::Write | StatementClass::Unknown) {
        return stream;
    }
    stream
        .map(|event| match event {
            Ok(ExecEvent::ResultEnd { truncated, .. }) => Ok(ExecEvent::ResultEnd {
                affected: None,
                truncated,
            }),
            other => other,
        })
        .boxed()
}
