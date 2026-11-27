//! The conformance suite: what every driver must do, checked against a real server.
//!
//! A driver's integration test builds a [`Fixture`] for its engine (a running container and the
//! statements to use) and calls [`assert_all`]. The checks:
//! connect and ping; the object tree; a round trip of every supported type; streaming a million
//! rows within the memory budget; cancelling within 2 s; read-only enforcement; error mapping
//! (authentication, network, syntax with position, permission, timeout); and reconnecting after
//! the server restarts.

use crate::memory::rss_bytes;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use treasury_core::analyze::StatementClass;
use treasury_core::cache::PageStore;
use treasury_core::config::{ConnectionConfig, Endpoint};
use treasury_core::cursor::{Execution, ExecutionOptions};
use treasury_core::driver::{ConnectContext, Driver, ReadOnlyEnforcement, SecretSource, Session};
use treasury_core::exec::ExecRequest;
use treasury_core::guard::{self, Decision, Policy};
use treasury_core::schema::{ChildQuery, ObjectPath};
use treasury_core::{Error, ErrorKind, Value};

/// A statement setup and the values a query must return, column by column.
pub struct TypeCases {
    /// Statements that create and fill the table.
    pub setup: Vec<String>,
    /// The query returning one row.
    pub query: String,
    /// Expected value per column, by column name.
    pub expected: Vec<(String, Value)>,
}

/// One engine under test.
#[async_trait]
pub trait Fixture: Send + Sync {
    /// The driver.
    fn driver(&self) -> Arc<dyn Driver>;
    /// Settings that reach the running server.
    fn config(&self) -> ConnectionConfig;
    /// The right credentials.
    fn secrets(&self) -> Arc<dyn SecretSource>;
    /// Credentials the server rejects.
    fn wrong_secrets(&self) -> Arc<dyn SecretSource>;
    /// A statement returning `n` rows of one integer column counting from 1 (or, where the
    /// engine keeps only strings, the numbers as text).
    fn rows(&self, n: u64) -> String;
    /// A statement that runs for at least `secs` seconds.
    fn slow(&self, secs: u32) -> String;
    /// A statement with a syntax error, and where the server says the error is.
    fn syntax_error(&self) -> (String, Option<u32>);
    /// A statement that writes.
    fn write(&self) -> String;
    /// The type round trip.
    fn types(&self) -> TypeCases;
    /// An object that exists once [`Fixture::types`] ran.
    fn known_object(&self) -> ObjectPath;
    /// A connection the server accepts but that may not run `statement`.
    fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)>;
    /// Restarts the server; existing connections break.
    async fn restart(&self);
}

fn context(secrets: Arc<dyn SecretSource>, read_only: bool) -> ConnectContext {
    ConnectContext {
        secrets,
        via: None,
        connect_timeout: Duration::from_secs(20),
        metadata_timeout: Duration::from_secs(20),
        read_only,
    }
}

async fn connect(f: &dyn Fixture, read_only: bool) -> Arc<dyn Session> {
    f.driver()
        .connect(&f.config(), context(f.secrets(), read_only))
        .await
        .unwrap_or_else(|e| panic!("connect failed: {e}"))
}

async fn run(session: &dyn Session, sql: &str) -> Result<Vec<Vec<Value>>, Error> {
    run_request(session, ExecRequest::new(sql)).await
}

async fn run_request(session: &dyn Session, req: ExecRequest) -> Result<Vec<Vec<Value>>, Error> {
    let exec = session
        .execute()
        .ok_or_else(|| Error::unsupported("execute"))?;
    let stream = exec.execute(req).await?;
    let dir = tempfile::tempdir().map_err(|e| Error::internal(e.to_string()))?;
    let store = PageStore::new(16 * 1024 * 1024, dir.path().into())?;
    let ex = Execution::new(stream, store, ExecutionOptions::default());
    // Read every result set in turn; keep the rows of the last one that had columns.
    let mut rows = Vec::new();
    let mut set = 0;
    loop {
        let w = ex.window(set, 0, usize::MAX / 2).await?;
        let status = ex.status();
        if status.sets.get(set).is_some_and(|s| !s.columns.is_empty()) {
            rows = w.rows;
        }
        if status.finished && status.sets.len() <= set + 1 {
            break;
        }
        set += 1;
    }
    if let Some(e) = ex.status().error {
        return Err(e);
    }
    Ok(rows)
}

/// Runs every check, then restarts the server for the reconnect check. Panics listing every
/// failed check.
pub async fn assert_all(f: &dyn Fixture) {
    let mut failures = Vec::new();
    macro_rules! check {
        ($name:literal, $e:expr) => {
            let started = Instant::now();
            match catching($e).await {
                Ok(()) => println!("ok    {} ({:.1}s)", $name, started.elapsed().as_secs_f64()),
                Err(msg) => {
                    println!("FAIL  {}: {}", $name, msg);
                    failures.push(format!("{}: {}", $name, msg));
                }
            }
        };
    }
    check!("connect and ping", connect_and_ping(f));
    check!("type round trip", type_round_trip(f));
    check!("object tree", object_tree(f));
    check!(
        "stream a million rows within the memory budget",
        stream_million_rows(f)
    );
    check!("cancel within 2 s", cancel(f));
    check!("read-only enforcement", read_only(f));
    check!("error mapping", error_mapping(f));
    check!("reconnect after a restart", reconnect(f));
    assert!(
        failures.is_empty(),
        "conformance failures:\n{}",
        failures.join("\n")
    );
}

/// Runs a check, turning a panic inside it into a failure message so the other checks still run.
async fn catching<F: std::future::Future<Output = Result<(), String>>>(f: F) -> Result<(), String> {
    use futures::FutureExt;
    match std::panic::AssertUnwindSafe(f).catch_unwind().await {
        Ok(r) => r,
        Err(p) => Err(p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "panicked".into())),
    }
}

macro_rules! ensure {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            return Err(format!($($fmt)*));
        }
    };
}

/// Connects, pings, reads server facts and closes.
pub async fn connect_and_ping(f: &dyn Fixture) -> Result<(), String> {
    let s = connect(f, false).await;
    let rtt = s.ping().await.map_err(|e| e.to_string())?;
    ensure!(rtt < Duration::from_secs(5), "ping took {rtt:?}");
    let info = s.server_info().await.map_err(|e| e.to_string())?;
    ensure!(!info.version.is_empty(), "no server version");
    s.close().await.map_err(|e| e.to_string())?;
    ensure!(s.ping().await.is_err(), "ping succeeded after close");
    Ok(())
}

/// Creates the type table and compares every value.
pub async fn type_round_trip(f: &dyn Fixture) -> Result<(), String> {
    let s = connect(f, false).await;
    let cases = f.types();
    for stmt in &cases.setup {
        run(s.as_ref(), stmt)
            .await
            .map_err(|e| format!("setup `{stmt}`: {e}"))?;
    }
    let exec = s.execute().ok_or("no execute")?;
    let stream = exec
        .execute(ExecRequest::new(&cases.query))
        .await
        .map_err(|e| e.to_string())?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let ex = Execution::new(
        stream,
        PageStore::new(1 << 24, dir.path().into()).map_err(|e| e.to_string())?,
        ExecutionOptions::default(),
    );
    let w = ex.window(0, 0, 10).await.map_err(|e| e.to_string())?;
    let columns = ex
        .status()
        .sets
        .first()
        .map(|s| s.columns.clone())
        .unwrap_or_default();
    let row = w.rows.first().ok_or("the type query returned no row")?;
    let mut wrong = Vec::new();
    for (name, expected) in &cases.expected {
        let Some(i) = columns.iter().position(|c| &c.name == name) else {
            wrong.push(format!("{name}: column missing"));
            continue;
        };
        if &row[i] != expected {
            wrong.push(format!("{name}: expected {expected:?}, got {:?}", row[i]));
        }
    }
    ensure!(wrong.is_empty(), "{}", wrong.join("; "));
    Ok(())
}

/// Walks the tree from the root to the known object and reads its details.
pub async fn object_tree(f: &dyn Fixture) -> Result<(), String> {
    let s = connect(f, false).await;
    let tree = s.introspect().ok_or("no introspection")?;
    let target = f.known_object();
    let mut at = ObjectPath::root();
    for part in &target.0 {
        let page = tree
            .children(
                &at,
                &ChildQuery {
                    filter: Some(part.name.clone()),
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .map_err(|e| format!("children of {at:?}: {e}"))?;
        let node = page
            .nodes
            .iter()
            .find(|n| n.name == part.name || n.path.leaf() == Some(part))
            .ok_or_else(|| format!("{} not found under {at:?}", part.name))?;
        at = node.path.clone();
    }
    let details = tree.details(&at).await.map_err(|e| e.to_string())?;
    ensure!(!details.columns.is_empty(), "{at:?} has no columns");
    Ok(())
}

/// Streams a million rows through a 16 MB page cache and checks memory stays bounded.
pub async fn stream_million_rows(f: &dyn Fixture) -> Result<(), String> {
    const N: u64 = 1_000_000;
    const BUDGET: usize = 16 * 1024 * 1024;
    let s = connect(f, true).await;
    let exec = s.execute().ok_or("no execute")?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = PageStore::new(BUDGET, dir.path().into()).map_err(|e| e.to_string())?;
    // "Idle" is a connection that has run something: client libraries set up pools, buffers and
    // background tasks on first use, which is not what this check measures.
    run(s.as_ref(), &f.rows(1_000))
        .await
        .map_err(|e| format!("warm-up: {e}"))?;
    let before = rss_bytes();
    let stream = exec
        .execute(ExecRequest::new(f.rows(N)))
        .await
        .map_err(|e| e.to_string())?;
    let ex = Execution::new(
        stream,
        store.clone(),
        ExecutionOptions {
            page_rows: 1_000,
            max_rows: None,
        },
    );
    let mut peak_rss = 0u64;
    let mut peak_cache = 0usize;
    // By the first quarter every buffer the engine's client keeps is in use, so from there on
    // memory must stay flat: rows that piled up would show as steady growth.
    const STEADY: u64 = N / 4;
    let mut steady = None;
    let mut start = 0u64;
    let mut last = Value::Null;
    while start < N {
        let w = ex
            .window(0, start, 10_000)
            .await
            .map_err(|e| e.to_string())?;
        if w.rows.is_empty() {
            break;
        }
        last = w
            .rows
            .last()
            .and_then(|r| r.first().cloned())
            .unwrap_or(Value::Null);
        start += w.rows.len() as u64;
        ensure!(
            store.stats().memory_bytes <= BUDGET * 2,
            "page cache {} bytes over budget",
            store.stats().memory_bytes
        );
        peak_cache = peak_cache.max(store.stats().memory_bytes);
        if let Some(r) = rss_bytes() {
            peak_rss = peak_rss.max(r);
            if start == STEADY {
                steady = Some(r);
            }
        }
    }
    ensure!(start == N, "read {start} rows, expected {N}");
    // Redis keeps strings, so its rows are the numbers as text.
    ensure!(
        matches!(&last, Value::I64(n) if *n == 1_000_000)
            || matches!(&last, Value::Text(t) if t == "1000000"),
        "last row was {last:?}"
    );
    if let (Some(b), Some(steady), true) = (before, steady, peak_rss > 0) {
        const MB: u64 = 1024 * 1024;
        let growth = peak_rss.saturating_sub(b);
        let later = peak_rss.saturating_sub(steady);
        // The page cache, plus what the engine's client library and the allocator keep, which
        // differs by engine but not with the number of rows.
        let allowed = BUDGET as u64 + 32 * MB;
        let allowed_later = 16 * MB;
        println!(
            "      rss before {} MB, at {STEADY} rows {} MB, peak {} MB; page cache peak {} MB",
            b >> 20,
            steady >> 20,
            peak_rss >> 20,
            peak_cache >> 20
        );
        ensure!(
            later <= allowed_later,
            "RSS grew by {} MB over the last {} rows, more than {} MB: rows are piling up",
            later >> 20,
            N - STEADY,
            allowed_later >> 20
        );
        ensure!(
            growth <= allowed,
            "RSS grew by {} MB, more than the {} MB budget",
            growth >> 20,
            allowed >> 20
        );
    }
    Ok(())
}

/// Starts a slow statement, cancels it and expects the error within 2 s.
pub async fn cancel(f: &dyn Fixture) -> Result<(), String> {
    let s = connect(f, true).await;
    let sql = f.slow(30);
    let runner = {
        let s = s.clone();
        tokio::spawn(async move { run(s.as_ref(), &sql).await })
    };
    tokio::time::sleep(Duration::from_millis(800)).await;
    let cancelled_at = Instant::now();
    s.cancel()
        .ok_or("no cancel")?
        .cancel()
        .await
        .map_err(|e| e.to_string())?;
    let result = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .map_err(|_| "the statement was still running 5 s after cancel".to_owned())?
        .map_err(|e| e.to_string())?;
    let took = cancelled_at.elapsed();
    ensure!(took <= Duration::from_secs(2), "cancel took {took:?}");
    match result {
        Err(e) if e.kind == ErrorKind::Cancelled => {}
        other => return Err(format!("expected a cancelled error, got {other:?}")),
    }
    // The session stays usable.
    run(s.as_ref(), &f.rows(1))
        .await
        .map_err(|e| format!("after cancel: {e}"))?;
    Ok(())
}

/// A read-only session refuses writes, by the server or by the guardrails.
pub async fn read_only(f: &dyn Fixture) -> Result<(), String> {
    let s = connect(f, true).await;
    s.set_read_only(true).await.map_err(|e| e.to_string())?;
    let write = f.write();
    match s.capabilities().read_only {
        ReadOnlyEnforcement::Server => {
            let r = run(s.as_ref(), &write).await;
            ensure!(
                matches!(&r, Err(e) if matches!(e.kind, ErrorKind::ReadOnly | ErrorKind::Permission { .. })),
                "the server ran a write on a read-only session: {r:?}"
            );
        }
        ReadOnlyEnforcement::Client => {
            let policy = Policy {
                connection_name: "t".into(),
                environment: treasury_core::config::Environment::Production,
                read_only: true,
                enforcement: ReadOnlyEnforcement::Client,
                count_threshold: guard::DEFAULT_COUNT_THRESHOLD,
            };
            let reviewed = guard::review(&policy, s.analyzer(), &write);
            ensure!(
                reviewed
                    .iter()
                    .all(|r| matches!(r.decision, Decision::Block { .. })),
                "the guardrails let a write through: {reviewed:?}"
            );
        }
    }
    let classes: Vec<StatementClass> = s
        .analyzer()
        .classify(&write)
        .iter()
        .map(|c| c.class)
        .collect();
    ensure!(
        classes.iter().all(|c| c.may_write()),
        "the write statement was classified as {classes:?}"
    );
    Ok(())
}

/// Authentication, network, syntax, permission and timeout errors map to their kinds.
pub async fn error_mapping(f: &dyn Fixture) -> Result<(), String> {
    match f
        .driver()
        .connect(&f.config(), context(f.wrong_secrets(), false))
        .await
    {
        Err(e) if e.kind == ErrorKind::Auth => {}
        other => {
            return Err(format!(
                "wrong password: expected an auth error, got {:?}",
                other.err()
            ));
        }
    }
    let mut nowhere = f.config();
    nowhere.endpoint = Endpoint::Host {
        host: "127.0.0.1".into(),
        port: 1,
    };
    match f
        .driver()
        .connect(&nowhere, context(f.secrets(), false))
        .await
    {
        Err(e) if matches!(e.kind, ErrorKind::Network { .. }) => {}
        other => {
            return Err(format!(
                "closed port: expected a network error, got {:?}",
                other.err()
            ));
        }
    }
    let s = connect(f, false).await;
    let (bad, position) = f.syntax_error();
    match run(s.as_ref(), &bad).await {
        Err(e) if e.kind == ErrorKind::Syntax => {
            if let Some(p) = position {
                let got = e.server.as_ref().and_then(|s| s.position);
                ensure!(
                    got == Some(p),
                    "syntax error position: expected {p}, got {got:?}"
                );
            }
        }
        other => return Err(format!("syntax: expected a syntax error, got {other:?}")),
    }
    if let Some((config, secrets, stmt)) = f.unprivileged() {
        let low = f
            .driver()
            .connect(&config, context(secrets, false))
            .await
            .map_err(|e| e.to_string())?;
        match run(low.as_ref(), &stmt).await {
            Err(e) if matches!(e.kind, ErrorKind::Permission { .. }) => {}
            other => {
                return Err(format!(
                    "permission: expected a permission error, got {other:?}"
                ));
            }
        }
    }
    let req = ExecRequest::new(f.slow(10)).with_timeout(Duration::from_secs(1));
    match run_request(s.as_ref(), req).await {
        Err(e) if matches!(e.kind, ErrorKind::Timeout { .. }) => {}
        other => return Err(format!("timeout: expected a timeout error, got {other:?}")),
    }
    Ok(())
}

/// After a restart the old session reports a lost connection and a new one works.
pub async fn reconnect(f: &dyn Fixture) -> Result<(), String> {
    let s = connect(f, false).await;
    f.restart().await;
    let old = s.ping().await;
    ensure!(
        old.is_err(),
        "the old session still answered after a restart"
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f
            .driver()
            .connect(&f.config(), context(f.secrets(), false))
            .await
        {
            Ok(fresh) => {
                fresh.ping().await.map_err(|e| e.to_string())?;
                return Ok(());
            }
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(format!("could not reconnect within 60 s: {e}")),
        }
    }
}
