//! The conformance suite run against the in-memory mock driver: proves the suite's own checks
//! work before real drivers rely on them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use std::sync::Arc;
use treasury_core::config::{ConnectionConfig, Engine};
use treasury_core::driver::{Driver, SecretSource};
use treasury_core::schema::{ObjectKind, ObjectPath};
use treasury_core::value::{Date, Value};
use treasury_tests::MapSecrets;
use treasury_tests::conformance::{Fixture, TypeCases, assert_all};
use treasury_tests::mock::{MockDriver, PASSWORD};

struct MockFixture {
    driver: MockDriver,
    broken_cancel: bool,
}

#[async_trait]
impl Fixture for MockFixture {
    fn driver(&self) -> Arc<dyn Driver> {
        Arc::new(self.driver.clone())
    }
    fn config(&self) -> ConnectionConfig {
        ConnectionConfig::new("mock", Engine::Postgres, "mock").unwrap()
    }
    fn secrets(&self) -> Arc<dyn SecretSource> {
        Arc::new(MapSecrets::password(PASSWORD))
    }
    fn wrong_secrets(&self) -> Arc<dyn SecretSource> {
        Arc::new(MapSecrets::password("wrong"))
    }
    fn rows(&self, n: u64) -> String {
        format!("rows {n}")
    }
    fn slow(&self, secs: u32) -> String {
        if self.broken_cancel {
            "rows 1".into()
        } else {
            format!("sleep {secs}")
        }
    }
    fn syntax_error(&self) -> (String, Option<u32>) {
        ("syntax".into(), Some(3))
    }
    fn write(&self) -> String {
        "write".into()
    }
    fn types(&self) -> TypeCases {
        TypeCases {
            setup: vec!["setup".into()],
            query: "types".into(),
            expected: vec![
                ("b".into(), Value::Bool(true)),
                ("i".into(), Value::I64(-42)),
                ("t".into(), Value::Text("héllo".into())),
                ("d".into(), Value::Date(Date { days: 19_782 })),
            ],
        }
    }
    fn known_object(&self) -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "main")
            .child(ObjectKind::Table, "types")
    }
    fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)> {
        Some((self.config(), self.secrets(), "denied".into()))
    }
    async fn restart(&self) {
        self.driver.restart();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mock_driver_passes() {
    assert_all(&MockFixture {
        driver: MockDriver::default(),
        broken_cancel: false,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "conformance failures")]
async fn the_suite_catches_a_driver_that_cannot_cancel() {
    assert_all(&MockFixture {
        driver: MockDriver::default(),
        broken_cancel: true,
    })
    .await;
}
