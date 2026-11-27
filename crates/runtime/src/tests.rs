#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use async_trait::async_trait;
use treasury_core::analyze::StatementClass;
use treasury_core::config::Engine;
use treasury_core::edit::RowChange;
use treasury_ipc::{CellEdit, RowEdits};
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
    driver: MockDriver,
    _dir: tempfile::TempDir,
}

fn runtime() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::open_in_memory(Sealer::new(zeroize::Zeroizing::new([7u8; 32]))).unwrap());
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let driver = MockDriver::default();
    let rt = Runtime::new(
        store,
        secrets,
        vec![Arc::new(driver.clone())],
        KnownHosts::new(dir.path().join("known_hosts")),
        Arc::new(Deny),
        dir.path().to_path_buf(),
    );
    Fixture {
        rt,
        driver,
        _dir: dir,
    }
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
    assert_eq!(
        (history[0].status, history[0].rows),
        (RunStatus::Ok, Some(25))
    );
}

/// The grid reads only the rows it shows, so most results are closed before their stream ends.
#[tokio::test]
async fn results_closed_before_the_end_are_remembered() {
    let f = runtime();
    let c = open(&f.rt, Environment::Development).await;
    let p = f.rt.prepare(&c.id, "rows 25").await.unwrap();
    let rid =
        f.rt.run(&c.id, "rows 25", p.token, None, true)
            .await
            .unwrap();
    // In the history from the start, as it runs.
    let running = f.rt.store().history(Some(&c.id), None, 10).unwrap();
    assert_eq!(running[0].status, RunStatus::Unfinished);
    f.rt.window(rid, 0, 0, 10).await.unwrap();
    // The time it took is how long the rows took to come, not how long the result stayed open.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    f.rt.close(rid);
    let done = f.rt.store().history(Some(&c.id), None, 10).unwrap();
    assert!(done[0].duration_ms < 250, "{} ms", done[0].duration_ms);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].status, RunStatus::Ok);
    assert!(done[0].rows.is_some_and(|n| n >= 10), "{:?}", done[0].rows);

    // Cancelled by the user, then closed.
    let p = f.rt.prepare(&c.id, "rows 25").await.unwrap();
    let rid =
        f.rt.run(&c.id, "rows 25", p.token, None, true)
            .await
            .unwrap();
    f.rt.cancel(rid).await.unwrap();
    f.rt.close(rid);
    assert_eq!(
        f.rt.store().history(Some(&c.id), None, 10).unwrap()[0].status,
        RunStatus::Cancelled
    );

    // Refused by the server.
    let p = f.rt.prepare(&c.id, "syntax x").await.unwrap();
    let rid =
        f.rt.run(&c.id, "syntax x", p.token, None, true)
            .await
            .unwrap();
    assert!(f.rt.window(rid, 0, 0, 10).await.is_err());
    assert_eq!(
        f.rt.store().history(Some(&c.id), None, 10).unwrap()[0].status,
        RunStatus::Failed
    );

    // Still open when Treasury quits.
    let p = f.rt.prepare(&c.id, "rows 3").await.unwrap();
    f.rt.run(&c.id, "rows 3", p.token, None, true)
        .await
        .unwrap();
    f.rt.close_all();
    let last = &f.rt.store().history(Some(&c.id), None, 10).unwrap()[0];
    assert_eq!((last.text.as_str(), last.status), ("rows 3", RunStatus::Ok));
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

#[tokio::test]
async fn only_writes_report_rows_changed() {
    let f = runtime();
    let c = open(&f.rt, Environment::Local).await;
    for (statement, affected) in [("ddl", None), ("write 1", Some(1))] {
        let p = f.rt.prepare(&c.id, statement).await.unwrap();
        let rid =
            f.rt.run(&c.id, statement, p.token, None, true)
                .await
                .unwrap();
        let status = finish(&f.rt, rid).await;
        assert_eq!(status.sets[0].affected, affected, "{statement}");
    }
}

/// Runs `statement` to its end and returns the result's id.
async fn run_all(f: &Fixture, c: &ConnectionConfig, statement: &str) -> u64 {
    let p = f.rt.prepare(&c.id, statement).await.unwrap();
    let rid =
        f.rt.run(&c.id, statement, p.token, None, true)
            .await
            .unwrap();
    finish(&f.rt, rid).await;
    rid
}

fn cell(row: u64, value: Option<&str>) -> CellEdit {
    CellEdit {
        row,
        column: 0,
        value: value.map(str::to_owned),
    }
}

#[tokio::test]
async fn edits_reach_rows_by_the_key_the_server_sent() {
    let f = runtime();
    let c = open(&f.rt, Environment::Development).await;
    let rid = run_all(&f, &c, "rows 5").await;
    let info = f.rt.edit_info(rid, 0).await.unwrap();
    assert_eq!(info.table.as_deref(), Some("numbers"));
    assert_eq!(info.columns, vec![true]);

    // Row 1 holds n = 2, row 3 holds n = 4. The second edit of a cell replaces the first, and a
    // row that is deleted needs no update.
    let edits = RowEdits {
        cells: vec![
            cell(1, Some("20")),
            cell(1, Some("22")),
            cell(3, Some("40")),
            cell(4, None),
        ],
        deletes: vec![3, 3],
    };
    assert_eq!(f.rt.edit_review(rid, 0, &edits).await.unwrap().len(), 3);
    assert_eq!(f.rt.edit_apply(rid, 0, &edits).await.unwrap(), 3);
    let applied = f.driver.applied();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].key_columns, vec!["n".to_owned()]);
    assert_eq!(
        applied[0].changes,
        vec![
            RowChange::Update {
                key: vec![Value::I64(2)],
                set: vec![("n".into(), Value::I64(22))],
            },
            RowChange::Update {
                key: vec![Value::I64(5)],
                set: vec![("n".into(), Value::Null)],
            },
            RowChange::Delete {
                key: vec![Value::I64(4)],
            },
        ]
    );

    let typo = RowEdits {
        cells: vec![cell(0, Some("two"))],
        deletes: Vec::new(),
    };
    let err = f.rt.edit_apply(rid, 0, &typo).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Config { .. }), "{err:?}");
    assert!(f.rt.edit_apply(rid, 0, &RowEdits::default()).await.is_err());
    assert_eq!(f.driver.applied().len(), 1, "nothing more was applied");
}

#[tokio::test]
async fn production_edits_wait_for_unlocked_writes() {
    let f = runtime();
    let c = open(&f.rt, Environment::Production).await;
    let rid = run_all(&f, &c, "rows 3").await;
    let edits = RowEdits {
        cells: vec![cell(0, Some("9"))],
        deletes: Vec::new(),
    };
    let err = f.rt.edit_apply(rid, 0, &edits).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Guard);
    assert!(f.driver.applied().is_empty());
    f.rt.unlock_writes(&c.id).await.unwrap();
    assert_eq!(f.rt.edit_apply(rid, 0, &edits).await.unwrap(), 1);
}

#[tokio::test]
async fn results_the_analyzer_cannot_place_say_why() {
    let f = runtime();
    let c = open(&f.rt, Environment::Local).await;
    let rid = run_all(&f, &c, "types").await;
    let info = f.rt.edit_info(rid, 0).await.unwrap();
    assert!(info.table.is_none());
    assert!(info.reason.is_some_and(|r| r.contains("rows N")));
    let err =
        f.rt.edit_apply(
            rid,
            0,
            &RowEdits {
                cells: vec![cell(0, Some("x"))],
                deletes: Vec::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Guard);
}

#[tokio::test]
async fn production_keeps_no_history_unless_asked_to() {
    let f = runtime();
    let prod = open(&f.rt, Environment::Production).await;
    run_all(&f, &prod, "rows 1").await;
    assert!(
        f.rt.store()
            .history(Some(&prod.id), None, 10)
            .unwrap()
            .is_empty()
    );

    let mut kept = ConnectionConfig::new("audited-prod", Engine::Postgres, "mock").unwrap();
    kept.environment = Environment::Production;
    kept.history = Some(true);
    f.rt.secrets_of(&kept.id)
        .set(
            treasury_core::config::SecretSlot::Password,
            &secrecy::SecretString::from(PASSWORD),
        )
        .unwrap();
    f.rt.connect(&kept).await.unwrap();
    run_all(&f, &kept, "rows 1").await;
    assert_eq!(
        f.rt.store()
            .history(Some(&kept.id), None, 10)
            .unwrap()
            .len(),
        1
    );
}
