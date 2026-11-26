#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use async_trait::async_trait;
use treasury_core::analyze::StatementClass;
use treasury_core::config::Engine;
use treasury_tests::mock::{MockDriver, PASSWORD};
use treasury_vault::{MemoryStore, Sealer};

struct Deny;

#[async_trait]
impl HostKeyPrompt for Deny {
    async fn confirm_new(&self, _: &str, _: u16, _: &str, _: &str) -> bool {
        false
    }
}

struct Fixture {
    rt: Runtime,
    _dir: tempfile::TempDir,
}

fn runtime() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::open_in_memory(Sealer::new(zeroize::Zeroizing::new([7u8; 32]))).unwrap());
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let rt = Runtime::new(
        store,
        secrets,
        vec![Arc::new(MockDriver::default())],
        KnownHosts::new(dir.path().join("known_hosts")),
        Arc::new(Deny),
        dir.path().to_path_buf(),
    );
    Fixture { rt, _dir: dir }
}

async fn open(rt: &Runtime, env: Environment) -> ConnectionConfig {
    let mut c = ConnectionConfig::new("orders-prod", Engine::Postgres, "mock").unwrap();
    c.environment = env;
    rt.secrets_of(&c.id)
        .set(
            treasury_core::config::SecretSlot::Password,
            &secrecy::SecretString::from(PASSWORD),
        )
        .unwrap();
    rt.connect(&c).await.unwrap();
    c
}

/// Reads the result to its end, as scrolling down would.
async fn finish(rt: &Runtime, rid: u64) -> ExecutionStatus {
    let mut start = 0;
    for _ in 0..1_000 {
        let s = rt.status(rid).unwrap();
        if s.finished {
            return s;
        }
        match rt.window(rid, 0, start, 100).await {
            Ok(w) if !w.rows.is_empty() => start += w.rows.len() as u64,
            // At the end of a set, asking for the next one reads the stream to its end.
            _ => {
                let next = rt.status(rid).unwrap().sets.len();
                let _ = rt.window(rid, next, 0, 1).await;
            }
        }
    }
    panic!("the statement never finished");
}

#[tokio::test]
async fn reads_run_and_are_remembered() {
    let f = runtime();
    let c = open(&f.rt, Environment::Development).await;
    let p = f.rt.prepare(&c.id, "rows 25").await.unwrap();
    assert_eq!(p.class, StatementClass::Read);
    assert!(p.refused.is_none() && p.name_required.is_none());
    let rid =
        f.rt.run(&c.id, "rows 25", p.token, None, true)
            .await
            .unwrap();
    let w = f.rt.window(rid, 0, 0, 10).await.unwrap();
    assert_eq!(w.rows.len(), 10);
    let status = finish(&f.rt, rid).await;
    assert_eq!(status.sets[0].rows, 25);
    f.rt.close(rid);
    let history = f.rt.store().history(Some(&c.id), None, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].text, "rows 25");
}

#[tokio::test]
async fn production_is_read_only_until_unlocked() {
    let f = runtime();
    let c = open(&f.rt, Environment::Production).await;
    let p = f.rt.prepare(&c.id, "write 1").await.unwrap();
    assert!(p.refused.is_some(), "{p:?}");
    // The review is repeated when running: a refused statement cannot be forced.
    assert_eq!(
        f.rt.run(&c.id, "write 1", p.token, None, true)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Guard
    );
    f.rt.unlock_writes(&c.id).await.unwrap();
    let p = f.rt.prepare(&c.id, "write 1").await.unwrap();
    assert!(p.refused.is_none(), "{p:?}");
    let rid =
        f.rt.run(&c.id, "write 1", p.token, None, true)
            .await
            .unwrap();
    let status = finish(&f.rt, rid).await;
    assert!(status.error.is_none(), "{:?}", status.error);
    f.rt.lock_writes(&c.id).await.unwrap();
    assert!(f.rt.live(&c.id).await.unwrap().read_only());
}

#[tokio::test]
async fn reviews_are_single_use_and_tied_to_the_statement() {
    let f = runtime();
    let c = open(&f.rt, Environment::Development).await;
    let p = f.rt.prepare(&c.id, "rows 1").await.unwrap();
    assert_eq!(
        f.rt.run(&c.id, "rows 2", p.token, None, true)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Guard,
        "a different statement must not run under another's review"
    );
    let p = f.rt.prepare(&c.id, "rows 1").await.unwrap();
    f.rt.run(&c.id, "rows 1", p.token, None, true)
        .await
        .unwrap();
    assert_eq!(
        f.rt.run(&c.id, "rows 1", p.token, None, true)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Guard,
        "a review is used once"
    );
    assert_eq!(
        f.rt.run(&c.id, "rows 1", 999_999, None, true)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Guard
    );
}

#[tokio::test]
async fn the_row_limit_stops_reading() {
    let f = runtime();
    let c = open(&f.rt, Environment::Development).await;
    f.rt.store().set_setting("row_limit", &50u64).unwrap();
    let p = f.rt.prepare(&c.id, "rows 500").await.unwrap();
    let rid =
        f.rt.run(&c.id, "rows 500", p.token, None, true)
            .await
            .unwrap();
    let status = finish(&f.rt, rid).await;
    assert_eq!(status.sets[0].rows, 50);
    assert!(status.sets[0].limited);
    let p = f.rt.prepare(&c.id, "rows 500").await.unwrap();
    let rid =
        f.rt.run(&c.id, "rows 500", p.token, None, false)
            .await
            .unwrap();
    assert_eq!(finish(&f.rt, rid).await.sets[0].rows, 500);
}

#[tokio::test]
async fn cancel_stops_a_running_statement() {
    let f = runtime();
    let c = open(&f.rt, Environment::Development).await;
    let p = f.rt.prepare(&c.id, "sleep 30").await.unwrap();
    let rid =
        f.rt.run(&c.id, "sleep 30", p.token, None, true)
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.rt.cancel(rid).await.unwrap();
    let status = finish(&f.rt, rid).await;
    assert!(status.error.is_some());
}
