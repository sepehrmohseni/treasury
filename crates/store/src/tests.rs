use super::*;
use secrecy::{ExposeSecret, SecretString};
use treasury_core::config::{Engine, Environment, SecretSlot};
use treasury_core::health::{Metric, MetricStatus, MetricValue};
use treasury_vault::{MemoryStore, random_key};

fn store() -> Store {
    Store::open_in_memory(Sealer::new(random_key().unwrap())).unwrap()
}

fn conn(name: &str) -> ConnectionConfig {
    ConnectionConfig::new(name, Engine::Postgres, "db.internal").unwrap()
}

fn entry(c: &ConnectionId, text: &str, at: i64) -> HistoryEntry {
    HistoryEntry {
        id: 0,
        connection: c.clone(),
        text: text.into(),
        executed_at: at,
        duration_ms: 12,
        rows: Some(3),
        status: RunStatus::Ok,
    }
}

#[test]
fn connections_keep_order_and_survive_updates() {
    let s = store();
    let a = conn("a");
    let mut b = conn("b");
    s.save_connection(&a, 1).unwrap();
    s.save_connection(&b, 2).unwrap();
    b.environment = Environment::Production;
    s.save_connection(&b, 3).unwrap();
    let list = s.connections().unwrap();
    assert_eq!(
        list.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(list[1].environment, Environment::Production);
    s.reorder_connections(&[b.id.clone(), a.id.clone()])
        .unwrap();
    assert_eq!(s.connections().unwrap()[0].name, "b");
    s.delete_connection(&a.id).unwrap();
    assert_eq!(s.connections().unwrap().len(), 1);
}

#[test]
fn history_text_is_sealed_at_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("treasury.db");
    let key = random_key().unwrap();
    let c = conn("a");
    {
        let s = Store::open(&path, Sealer::new(key.clone())).unwrap();
        s.save_connection(&c, 1).unwrap();
        s.add_history(&entry(&c.id, "select needle_in_history from t", 1))
            .unwrap();
        s.save_query(
            &SavedQuery {
                id: "q1".into(),
                folder: "".into(),
                name: "n".into(),
                connection: None,
                updated_at: 1,
            },
            "select needle_in_saved",
        )
        .unwrap();
    }
    let mut raw = std::fs::read(&path).unwrap();
    if let Ok(wal) = std::fs::read(dir.path().join("treasury.db-wal")) {
        raw.extend(wal);
    }
    assert!(
        !raw.windows(6).any(|w| w == b"needle"),
        "plaintext query text on disk"
    );
    let s = Store::open(&path, Sealer::new(key)).unwrap();
    assert_eq!(
        s.history(None, None, 10).unwrap()[0].text,
        "select needle_in_history from t"
    );
    assert_eq!(
        s.saved_query_text("q1").unwrap().as_deref(),
        Some("select needle_in_saved")
    );
    // A different key cannot read it.
    drop(s);
    let other = Store::open(&path, Sealer::new(random_key().unwrap())).unwrap();
    assert!(matches!(
        other.history(None, None, 10),
        Err(StoreError::Sealed)
    ));
}

#[cfg(unix)]
#[test]
fn the_database_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let _s = Store::open(&path, Sealer::new(random_key().unwrap())).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn history_pages_newest_first() {
    let s = store();
    let c = conn("a");
    for i in 0..5 {
        s.add_history(&entry(&c.id, &format!("select {i}"), i))
            .unwrap();
    }
    let page = s.history(Some(&c.id), None, 2).unwrap();
    assert_eq!(
        page.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
        ["select 4", "select 3"]
    );
    let next = s.history(Some(&c.id), Some(page[1].id), 2).unwrap();
    assert_eq!(next[0].text, "select 2");
    assert!(
        s.history(Some(&ConnectionId("other".into())), None, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn history_search_uses_words_and_prefixes() {
    let s = store();
    let a = conn("a");
    let b = conn("b");
    s.add_history(&entry(
        &a.id,
        "select * from orders where status = 'open'",
        1,
    ))
    .unwrap();
    s.add_history(&entry(&a.id, "select count(*) from customers", 2))
        .unwrap();
    s.add_history(&entry(&b.id, "delete from orders where id = 1", 3))
        .unwrap();
    let texts = |v: Vec<HistoryEntry>| v.into_iter().map(|e| e.text).collect::<Vec<_>>();
    assert_eq!(
        texts(s.search_history(None, "orders", 10).unwrap()).len(),
        2
    );
    assert_eq!(
        texts(s.search_history(Some(&a.id), "ord", 10).unwrap()),
        vec!["select * from orders where status = 'open'".to_owned()]
    );
    assert_eq!(
        texts(s.search_history(None, "orders delete", 10).unwrap()).len(),
        1
    );
    // Entries added after the index exists are searchable too.
    s.add_history(&entry(&b.id, "update invoices set paid = true", 4))
        .unwrap();
    assert_eq!(
        texts(s.search_history(None, "invoices", 10).unwrap()).len(),
        1
    );
    // FTS syntax in the input is taken literally rather than failing.
    assert!(s.search_history(None, "\"unbalanced OR NEAR(", 10).is_ok());
    // No words: plain recent history.
    assert_eq!(s.search_history(None, "  ", 10).unwrap().len(), 4);
}

#[test]
fn purge_removes_history() {
    let s = store();
    let a = conn("a");
    let b = conn("b");
    s.add_history(&entry(&a.id, "select 1", 1)).unwrap();
    s.add_history(&entry(&b.id, "select 2", 2)).unwrap();
    assert_eq!(s.search_history(None, "select", 10).unwrap().len(), 2);
    assert_eq!(s.purge_history(Some(&a.id)).unwrap(), 1);
    assert_eq!(s.search_history(None, "select", 10).unwrap().len(), 1);
    assert_eq!(s.purge_history(None).unwrap(), 1);
    assert!(s.history(None, None, 10).unwrap().is_empty());
}

#[test]
fn saved_queries_and_settings() {
    let s = store();
    let q = SavedQuery {
        id: "1".into(),
        folder: "reports/monthly".into(),
        name: "Revenue".into(),
        connection: None,
        updated_at: 5,
    };
    s.save_query(&q, "select sum(total) from orders").unwrap();
    assert_eq!(s.saved_queries().unwrap(), vec![q.clone()]);
    s.delete_saved_query("1").unwrap();
    assert!(s.saved_queries().unwrap().is_empty());
    assert_eq!(s.setting::<u32>("row_limit").unwrap(), None);
    s.set_setting("row_limit", &500u32).unwrap();
    s.set_setting("row_limit", &1000u32).unwrap();
    assert_eq!(s.setting::<u32>("row_limit").unwrap(), Some(1000));
}

#[test]
fn tabs_round_trip_and_close() {
    let s = store();
    let t = TabState {
        id: "t1".into(),
        position: 0,
        connection: Some(ConnectionId("c".into())),
        title: "Query 1".into(),
        text: "select 1;\n-- unsaved work".into(),
        cursor: 9,
        saved_query: None,
    };
    s.save_tab(&t).unwrap();
    let mut t2 = t.clone();
    t2.text.push_str("\nselect 2;");
    s.save_tab(&t2).unwrap();
    assert_eq!(s.tabs().unwrap(), vec![t2]);
    s.close_tab("t1").unwrap();
    assert!(s.tabs().unwrap().is_empty());
}

#[test]
fn snapshots_keep_the_newest() {
    let s = store();
    let c = ConnectionId("c".into());
    let m = |n| {
        vec![Metric {
            id: "size".into(),
            label: "Size".into(),
            group: "Storage".into(),
            status: MetricStatus::Ok {
                value: MetricValue::Bytes(n),
            },
        }]
    };
    for i in 0..5 {
        s.add_snapshot(&c, 100 + i, &m(u64::try_from(i).unwrap()), 3)
            .unwrap();
    }
    let (at, metrics) = s.snapshot_before(&c, 1_000).unwrap().unwrap();
    assert_eq!(at, 104);
    assert_eq!(metrics, m(4));
    assert_eq!(s.snapshot_before(&c, 103).unwrap().unwrap().0, 102);
    assert!(
        s.snapshot_before(&c, 101).unwrap().is_none(),
        "older ones were pruned"
    );
}

#[test]
fn the_field_key_is_created_once() {
    let secrets = MemoryStore::default();
    let k1 = field_key(&secrets).unwrap();
    let k2 = field_key(&secrets).unwrap();
    assert_eq!(*k1, *k2);
}

#[test]
fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let key = random_key().unwrap();
    Store::open(&path, Sealer::new(key.clone())).unwrap();
    Store::open(&path, Sealer::new(key)).unwrap();
}

#[test]
fn fts_query_quotes_words() {
    assert_eq!(
        fts_query("orders open").as_deref(),
        Some("\"orders\" \"open\"*")
    );
    assert_eq!(
        fts_query("a\"b OR c").as_deref(),
        Some("\"a\" \"b\" \"OR\" \"c\"*")
    );
    assert_eq!(fts_query(" !? "), None);
}

mod transfer_tests {
    use super::*;
    use crate::transfer;

    #[test]
    fn plain_export_has_no_secrets() {
        let secrets = MemoryStore::default();
        let c = conn("prod");
        secrets
            .set(
                &SecretSlot::Password.key(&c.id),
                &SecretString::from("hunter2".to_owned()),
            )
            .unwrap();
        let bytes = transfer::export(std::slice::from_ref(&c), &secrets, None).unwrap();
        assert!(!transfer::needs_passphrase(&bytes));
        assert!(!bytes.windows(7).any(|w| w == b"hunter2"));
        let imported = transfer::import(&bytes, None).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].config.name, "prod");
        assert_ne!(imported[0].config.id, c.id, "imports get new ids");
        assert!(imported[0].secrets.is_empty());
    }

    #[test]
    fn sealed_export_carries_secrets() {
        let secrets = MemoryStore::default();
        let c = conn("prod");
        secrets
            .set(
                &SecretSlot::Password.key(&c.id),
                &SecretString::from("hunter2".to_owned()),
            )
            .unwrap();
        let pass = SecretString::from("a long passphrase".to_owned());
        let bytes = transfer::export(std::slice::from_ref(&c), &secrets, Some(&pass)).unwrap();
        assert!(transfer::needs_passphrase(&bytes));
        assert!(!bytes.windows(7).any(|w| w == b"hunter2"));
        assert!(matches!(
            transfer::import(&bytes, None),
            Err(StoreError::NeedsPassphrase)
        ));
        assert!(transfer::import(&bytes, Some(&SecretString::from("wrong".to_owned()))).is_err());
        let imported = transfer::import(&bytes, Some(&pass)).unwrap();
        assert_eq!(imported[0].secrets.len(), 1);
        assert_eq!(imported[0].secrets[0].0, SecretSlot::Password);
        assert_eq!(imported[0].secrets[0].1.expose_secret(), "hunter2");
    }

    #[test]
    fn foreign_files_are_refused() {
        assert!(
            transfer::import(
                b"{\"format\":\"other\",\"version\":1,\"connections\":[]}",
                None
            )
            .is_err()
        );
        assert!(transfer::import(b"not json", None).is_err());
        assert!(
            transfer::import(
                b"{\"format\":\"treasury-connections\",\"version\":99,\"connections\":[]}",
                None
            )
            .is_err()
        );
    }
}
