//! The MySQL and MariaDB driver against real servers in containers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod docker {
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, GenericImage, ImageExt};
    use treasury_core::config::{ConnectionConfig, Endpoint, Engine, TlsMode};
    use treasury_core::driver::{ConnectContext, Driver, SecretSource, Session};
    use treasury_core::edit::{Changeset, RowChange, TableRef};
    use treasury_core::exec::{ExecEvent, ExecRequest, Param};
    use treasury_core::health::{MetricSet, MetricStatus};
    use treasury_core::plan::ExplainMode;
    use treasury_core::schema::{ChildQuery, ObjectKind, ObjectPath};
    use treasury_core::value::{Blob, Date, Json, Time, Timestamp, TimestampTz, Value};
    use treasury_core::{ErrorKind, Result};
    use treasury_driver_mysql::MySqlDriver;
    use treasury_tests::MapSecrets;
    use treasury_tests::conformance::{Fixture, TypeCases, assert_all};
    use treasury_tests::docker::with_startup;

    const PASSWORD: &str = "conformance-password";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Flavor {
        MySql,
        MariaDb,
    }

    impl Flavor {
        fn image(self) -> (&'static str, String) {
            match self {
                Flavor::MySql => (
                    "mysql",
                    std::env::var("TREASURY_MYSQL_TAG").unwrap_or_else(|_| "9.7.0".into()),
                ),
                Flavor::MariaDb => (
                    "mariadb",
                    std::env::var("TREASURY_MARIADB_TAG").unwrap_or_else(|_| "12.3.2".into()),
                ),
            }
        }

        fn driver(self) -> MySqlDriver {
            match self {
                Flavor::MySql => MySqlDriver::mysql(),
                Flavor::MariaDb => MySqlDriver::mariadb(),
            }
        }

        fn engine(self) -> Engine {
            match self {
                Flavor::MySql => Engine::MySql,
                Flavor::MariaDb => Engine::MariaDb,
            }
        }
    }

    struct My {
        flavor: Flavor,
        container: ContainerAsync<GenericImage>,
        host: String,
        port: Mutex<u16>,
    }

    impl My {
        async fn start(flavor: Flavor) -> Self {
            let (name, tag) = flavor.image();
            let prefix = if flavor == Flavor::MySql {
                "MYSQL"
            } else {
                "MARIADB"
            };
            let container = with_startup(
                GenericImage::new(name, &tag)
                    .with_exposed_port(3306.tcp())
                    .with_wait_for(WaitFor::message_on_stderr("port: 3306")),
            )
            .with_env_var(format!("{prefix}_ROOT_PASSWORD"), PASSWORD)
            .with_env_var(format!("{prefix}_DATABASE"), "treasury")
            .start()
            .await
            .expect("start the server");
            let host = container.get_host().await.unwrap().to_string();
            let port = container.get_host_port_ipv4(3306.tcp()).await.unwrap();
            let my = Self {
                flavor,
                container,
                host,
                port: Mutex::new(port),
            };
            my.wait_ready().await;
            let s = my.session().await;
            for sql in [
                "CREATE USER 'low'@'%' IDENTIFIED BY 'low-password'",
                "CREATE TABLE secret_t (x int)",
                "CREATE TABLE ro_probe (x int)",
            ] {
                run(s.as_ref(), sql)
                    .await
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            my
        }

        async fn wait_ready(&self) {
            for _ in 0..240 {
                if self
                    .flavor
                    .driver()
                    .connect(&self.config(), ctx(self.secrets()))
                    .await
                    .is_ok()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            panic!("the server did not accept connections");
        }

        async fn session(&self) -> Arc<dyn Session> {
            self.flavor
                .driver()
                .connect(&self.config(), ctx(self.secrets()))
                .await
                .unwrap()
        }

        fn config_for(&self, user: &str, database: Option<&str>) -> ConnectionConfig {
            let mut c = ConnectionConfig::new("my", self.flavor.engine(), &self.host).unwrap();
            c.endpoint = Endpoint::Host {
                host: self.host.clone(),
                port: *self.port.lock().unwrap(),
            };
            c.user = Some(user.into());
            c.database = database.map(str::to_owned);
            c.tls.mode = TlsMode::Disable;
            c
        }
    }

    fn ctx(secrets: Arc<dyn SecretSource>) -> ConnectContext {
        ConnectContext {
            secrets,
            via: None,
            connect_timeout: Duration::from_secs(10),
            metadata_timeout: Duration::from_secs(10),
            read_only: false,
        }
    }

    async fn run(s: &dyn Session, sql: &str) -> Result<Vec<Vec<Value>>> {
        run_req(s, ExecRequest::new(sql))
            .await
            .map(|(rows, _)| rows)
    }

    async fn run_req(
        s: &dyn Session,
        req: ExecRequest,
    ) -> Result<(Vec<Vec<Value>>, Vec<ExecEvent>)> {
        let mut stream = s.execute().unwrap().execute(req).await?;
        let mut rows = Vec::new();
        let mut events = Vec::new();
        while let Some(e) = stream.next().await {
            match e? {
                ExecEvent::Rows(r) => rows.extend(r),
                other => events.push(other),
            }
        }
        Ok((rows, events))
    }

    #[async_trait]
    impl Fixture for My {
        fn driver(&self) -> Arc<dyn Driver> {
            Arc::new(self.flavor.driver())
        }
        fn config(&self) -> ConnectionConfig {
            self.config_for("root", Some("treasury"))
        }
        fn secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password(PASSWORD))
        }
        fn wrong_secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password("wrong"))
        }
        fn rows(&self, n: u64) -> String {
            format!(
                "WITH d(x) AS (SELECT 0 UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
                 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
                 UNION ALL SELECT 8 UNION ALL SELECT 9) \
                 SELECT a.x + 10 * b.x + 100 * c.x + 1000 * e.x + 10000 * f.x + 100000 * g.x + 1 AS n \
                 FROM d a, d b, d c, d e, d f, d g ORDER BY n LIMIT {n}"
            )
        }
        fn slow(&self, secs: u32) -> String {
            format!("SELECT SLEEP({secs})")
        }
        fn syntax_error(&self) -> (String, Option<u32>) {
            ("SELECT * FORM t".into(), Some(9))
        }
        fn write(&self) -> String {
            "INSERT INTO ro_probe VALUES (1)".into()
        }
        fn types(&self) -> TypeCases {
            type_cases(self.flavor)
        }
        fn known_object(&self) -> ObjectPath {
            ObjectPath::root()
                .child(ObjectKind::Database, "treasury")
                .child(ObjectKind::Group, "Tables")
                .child(ObjectKind::Table, "types")
        }
        fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)> {
            Some((
                self.config_for("low", None),
                Arc::new(MapSecrets::password("low-password")),
                "SELECT * FROM treasury.secret_t".into(),
            ))
        }
        async fn restart(&self) {
            self.container.stop().await.unwrap();
            self.container.start().await.unwrap();
            let port = self.container.get_host_port_ipv4(3306.tcp()).await.unwrap();
            *self.port.lock().unwrap() = port;
        }
    }

    /// 2024-02-29 13:45:30 as seconds since the epoch.
    const LEAP_DAY: i64 = 1_709_214_330;

    fn type_cases(flavor: Flavor) -> TypeCases {
        TypeCases {
            setup: vec![
                "DROP TABLE IF EXISTS types".into(),
                "CREATE TABLE types (id INT PRIMARY KEY, b BOOLEAN, ti TINYINT, si SMALLINT, i INT, \
                 bi BIGINT, ubi BIGINT UNSIGNED, f FLOAT, d DOUBLE, n DECIMAL(20,4), vc VARCHAR(20), \
                 c CHAR(3), t TEXT, bin VARBINARY(4), bl BLOB, dt DATE, tm TIME(1), dtm DATETIME(6), \
                 ts TIMESTAMP NULL, y YEAR, j JSON, e ENUM('a','b'), st SET('x','y'), bt BIT(8), nul INT)"
                    .into(),
                "INSERT INTO types VALUES (1, TRUE, -1, -2, -4, -8, 18446744073709551615, 1.5, 2.25, \
                 12345.6789, 'héllo', 'ab', 'text', 0x01FF, 0x00, '2024-02-29', '13:45:30.5', \
                 '2024-02-29 13:45:30.123456', '2024-02-29 13:45:30', 2024, \
                 '{\"b\": 1, \"a\": [1, 2]}', 'b', 'x,y', b'10100101', NULL)"
                    .into(),
            ],
            query: "SELECT * FROM types".into(),
            expected: expected_types(flavor),
        }
    }

    fn expected_types(flavor: Flavor) -> Vec<(String, Value)> {
        let json = match flavor {
            // MySQL stores JSON in a binary form and normalises key order.
            Flavor::MySql => Value::Json(Json {
                text: "{\"a\": [1, 2], \"b\": 1}".into(),
                binary: true,
            }),
            // MariaDB's JSON is text with a validity check.
            Flavor::MariaDb => Value::Text("{\"b\": 1, \"a\": [1, 2]}".into()),
        };
        vec![
            ("id".into(), Value::I64(1)),
            ("b".into(), Value::I64(1)),
            ("ti".into(), Value::I64(-1)),
            ("si".into(), Value::I64(-2)),
            ("i".into(), Value::I64(-4)),
            ("bi".into(), Value::I64(-8)),
            ("ubi".into(), Value::U64(u64::MAX)),
            ("f".into(), Value::F32(1.5)),
            ("d".into(), Value::F64(2.25)),
            ("n".into(), Value::Decimal("12345.6789".into())),
            ("vc".into(), Value::Text("héllo".into())),
            ("c".into(), Value::Text("ab".into())),
            ("t".into(), Value::Text("text".into())),
            ("bin".into(), Value::Bytes(Blob::full(vec![1, 255]))),
            ("bl".into(), Value::Bytes(Blob::full(vec![0]))),
            ("dt".into(), Value::Date(Date { days: 19_782 })),
            (
                "tm".into(),
                Value::Time(Time {
                    nanos: (13 * 3600 + 45 * 60 + 30) * 1_000_000_000 + 500_000_000,
                }),
            ),
            (
                "dtm".into(),
                Value::Timestamp(Timestamp {
                    secs: LEAP_DAY,
                    nanos: 123_456_000,
                }),
            ),
            (
                "ts".into(),
                Value::TimestampTz(TimestampTz {
                    utc: Timestamp {
                        secs: LEAP_DAY,
                        nanos: 0,
                    },
                    offset_secs: 0,
                }),
            ),
            ("y".into(), Value::I64(2024)),
            ("j".into(), json),
            ("e".into(), Value::Text("b".into())),
            ("st".into(), Value::Text("x,y".into())),
            ("bt".into(), Value::Bytes(Blob::full(vec![0xa5]))),
            ("nul".into(), Value::Null),
        ]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mysql_passes_conformance() {
        assert_all(&My::start(Flavor::MySql).await).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mariadb_passes_conformance() {
        assert_all(&My::start(Flavor::MariaDb).await).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mysql_specifics() {
        specifics(Flavor::MySql).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mariadb_specifics() {
        specifics(Flavor::MariaDb).await;
    }

    async fn specifics(flavor: Flavor) {
        let my = My::start(flavor).await;
        let s = my.session().await;
        for sql in type_cases(flavor).setup {
            run(s.as_ref(), &sql).await.unwrap();
        }
        statements_run_one_at_a_time(s.as_ref()).await;
        parameters_bind(s.as_ref()).await;
        warnings_become_notices(s.as_ref()).await;
        ddl_round_trips(s.as_ref()).await;
        tree_pages_and_filters(s.as_ref()).await;
        edits_are_atomic(s.as_ref()).await;
        rows_are_found_by_their_key(s.as_ref()).await;
        edits_join_an_open_transaction(&my).await;
        plans_are_trees(s.as_ref()).await;
        a_finished_statement_leaves_no_timer(s.as_ref()).await;
        metrics_load(&my, s.as_ref()).await;
    }

    async fn statements_run_one_at_a_time(s: &dyn Session) {
        let err = run(s, "SELECT 1; DROP TABLE types").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
        assert_eq!(
            run(s, "SELECT COUNT(*) FROM types").await.unwrap(),
            vec![vec![Value::I64(1)]],
            "the second statement must not have run"
        );
        let (_, events) = run_req(s, ExecRequest::new("UPDATE types SET t = t WHERE id = 1"))
            .await
            .unwrap();
        assert!(
            events.contains(&ExecEvent::ResultEnd {
                affected: Some(1),
                truncated: false
            }),
            "{events:?}"
        );
    }

    async fn parameters_bind(s: &dyn Session) {
        let mut req = ExecRequest::new("SELECT COUNT(*) FROM types WHERE dt = ? AND vc = ?");
        req.params = vec![
            Param {
                name: None,
                value: Value::Text("2024-02-29".into()),
            },
            Param {
                name: None,
                value: Value::Text("héllo".into()),
            },
        ];
        let (rows, _) = run_req(s, req).await.unwrap();
        assert_eq!(rows, vec![vec![Value::I64(1)]]);
        let mut req = ExecRequest::new("SELECT id FROM types WHERE dtm = :at");
        req.params = vec![Param {
            name: Some(":at".into()),
            value: Value::Timestamp(Timestamp {
                secs: LEAP_DAY,
                nanos: 123_456_000,
            }),
        }];
        let (rows, _) = run_req(s, req).await.unwrap();
        assert_eq!(rows, vec![vec![Value::I64(1)]]);
    }

    async fn warnings_become_notices(s: &dyn Session) {
        let (_, events) = run_req(s, ExecRequest::new("SELECT CAST('12abc' AS SIGNED)"))
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExecEvent::Notice(n) if n.message.contains("12abc"))),
            "{events:?}"
        );
    }

    async fn ddl_round_trips(s: &dyn Session) {
        for sql in [
            "CREATE TABLE customers (id INT AUTO_INCREMENT PRIMARY KEY, email VARCHAR(100) NOT NULL UNIQUE) \
             COMMENT 'People'",
            "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id INT NOT NULL, \
             total DECIMAL(12,2) NOT NULL DEFAULT 0, placed DATE, \
             CONSTRAINT orders_customer FOREIGN KEY (customer_id) REFERENCES customers (id), \
             INDEX orders_placed (placed))",
            "CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100",
            "CREATE FUNCTION twice(x INT) RETURNS INT DETERMINISTIC RETURN x * 2",
        ] {
            run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let tree = s.introspect().unwrap();
        let db = ObjectPath::root().child(ObjectKind::Database, "treasury");
        let objects = [
            db.child(ObjectKind::Table, "customers"),
            db.child(ObjectKind::Table, "orders"),
            db.child(ObjectKind::View, "big_orders"),
            db.child(ObjectKind::Function, "twice"),
        ];
        let mut ddl = Vec::new();
        for o in &objects {
            ddl.push(
                tree.ddl(o)
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("{o:?}: no DDL")),
            );
        }
        for sql in [
            "DROP FUNCTION twice",
            "DROP VIEW big_orders",
            "DROP TABLE orders",
            "DROP TABLE customers",
        ] {
            run(s, sql).await.unwrap();
        }
        for text in &ddl {
            run(s, text.trim_end_matches(';'))
                .await
                .unwrap_or_else(|e| panic!("replaying `{text}`: {e}"));
        }
        for (o, before) in objects.iter().zip(&ddl) {
            assert_eq!(&tree.ddl(o).await.unwrap().unwrap(), before, "{o:?}");
        }
        assert!(ddl[0].contains("COMMENT='People'"), "{}", ddl[0]);
        let details = tree.details(&objects[1]).await.unwrap();
        assert_eq!(details.primary_key(), vec!["id"]);
        assert_eq!(details.foreign_keys[0].ref_table, "customers");
        assert!(details.indexes.iter().any(|i| i.name == "orders_placed"));
    }

    async fn tree_pages_and_filters(s: &dyn Session) {
        for i in 0..25 {
            run(s, &format!("CREATE TABLE page_{i:02} (x int)"))
                .await
                .unwrap();
        }
        let tree = s.introspect().unwrap();
        let tables = ObjectPath::root()
            .child(ObjectKind::Database, "treasury")
            .child(ObjectKind::Group, "Tables");
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = tree
                .children(
                    &tables,
                    &ChildQuery {
                        filter: Some("PAGE_".into()),
                        cursor: cursor.clone(),
                        limit: 10,
                    },
                )
                .await
                .unwrap();
            assert!(page.nodes.len() <= 10);
            seen.extend(page.nodes.into_iter().map(|n| n.name));
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        let expected: Vec<String> = (0..25).map(|i| format!("page_{i:02}")).collect();
        assert_eq!(seen, expected);
        let none = tree
            .children(
                &tables,
                &ChildQuery {
                    filter: Some("page_0_".into()),
                    cursor: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(none.nodes.is_empty());
        let cols = tree
            .children(
                &ObjectPath::root()
                    .child(ObjectKind::Database, "treasury")
                    .child(ObjectKind::Table, "types")
                    .child(ObjectKind::Group, "Columns"),
                &ChildQuery {
                    limit: 3,
                    ..ChildQuery::default()
                },
            )
            .await
            .unwrap();
        let names: Vec<&str> = cols.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["id", "b", "ti"]);
    }

    fn change(changes: Vec<RowChange>) -> Changeset {
        Changeset {
            table: TableRef {
                schema: Some("treasury".into()),
                table: "edit_t".into(),
            },
            key_columns: vec!["id".into()],
            changes,
        }
    }

    async fn edits_are_atomic(s: &dyn Session) {
        run(
            s,
            "CREATE TABLE edit_t (id INT PRIMARY KEY, note TEXT, at DATE)",
        )
        .await
        .unwrap();
        run(
            s,
            "INSERT INTO edit_t VALUES (1, 'one', '2024-01-01'), (2, 'two', NULL)",
        )
        .await
        .unwrap();
        let edit = s.edit().unwrap();
        let ok = change(vec![
            RowChange::Update {
                key: vec![Value::I64(1)],
                set: vec![
                    ("note".into(), Value::Text("uno".into())),
                    ("at".into(), Value::Text("2025-12-31".into())),
                ],
            },
            // Setting a value it already has still counts as finding the row.
            RowChange::Update {
                key: vec![Value::I64(2)],
                set: vec![("note".into(), Value::Text("two".into()))],
            },
            RowChange::Insert {
                values: vec![
                    ("id".into(), Value::I64(3)),
                    ("note".into(), Value::Text("three".into())),
                ],
            },
            RowChange::Delete {
                key: vec![Value::I64(2)],
            },
        ]);
        assert_eq!(edit.apply(&ok).await.unwrap(), 4);
        assert_eq!(
            run(
                s,
                "SELECT id, note, CAST(at AS CHAR) FROM edit_t ORDER BY id"
            )
            .await
            .unwrap(),
            vec![
                vec![
                    Value::I64(1),
                    Value::Text("uno".into()),
                    Value::Text("2025-12-31".into())
                ],
                vec![Value::I64(3), Value::Text("three".into()), Value::Null],
            ]
        );
        let stale = change(vec![
            RowChange::Update {
                key: vec![Value::I64(3)],
                set: vec![("note".into(), Value::Text("changed".into()))],
            },
            RowChange::Delete {
                key: vec![Value::I64(99)],
            },
        ]);
        assert_eq!(edit.apply(&stale).await.unwrap_err().kind, ErrorKind::Guard);
        assert_eq!(
            run(s, "SELECT note FROM edit_t WHERE id = 3")
                .await
                .unwrap(),
            vec![vec![Value::Text("three".into())]]
        );
    }

    async fn edits_join_an_open_transaction(my: &My) {
        let s = my.session().await;
        run(s.as_ref(), "START TRANSACTION").await.unwrap();
        s.edit()
            .unwrap()
            .apply(&change(vec![RowChange::Update {
                key: vec![Value::I64(1)],
                set: vec![("note".into(), Value::Text("inside".into()))],
            }]))
            .await
            .unwrap();
        let other = my.session().await;
        assert_eq!(
            run(other.as_ref(), "SELECT note FROM edit_t WHERE id = 1")
                .await
                .unwrap(),
            vec![vec![Value::Text("uno".into())]]
        );
        run(s.as_ref(), "ROLLBACK").await.unwrap();
        assert_eq!(
            run(other.as_ref(), "SELECT note FROM edit_t WHERE id = 1")
                .await
                .unwrap(),
            vec![vec![Value::Text("uno".into())]],
            "rolling back the user's transaction must undo the edit"
        );
    }

    async fn plans_are_trees(s: &dyn Session) {
        let ex = s.explain().unwrap();
        let plan = ex
            .explain(
                "SELECT * FROM types t JOIN edit_t e ON e.id = t.id",
                ExplainMode::Estimate,
            )
            .await
            .unwrap();
        assert!(!plan.root.operator.is_empty());
        let analyzed = ex
            .explain("SELECT COUNT(*) FROM types", ExplainMode::Analyze)
            .await
            .unwrap();
        assert!(!analyzed.raw.is_empty());
        assert!(
            ex.explain("SELECT 1; DROP TABLE types", ExplainMode::Estimate)
                .await
                .is_err()
        );
        assert_eq!(
            run(s, "SELECT COUNT(*) FROM types").await.unwrap(),
            vec![vec![Value::I64(1)]]
        );
    }

    async fn a_finished_statement_leaves_no_timer(s: &dyn Session) {
        run_req(
            s,
            ExecRequest::new("SELECT 1").with_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap();
        let (rows, _) = run_req(s, ExecRequest::new("SELECT SLEEP(1.5), 7"))
            .await
            .unwrap();
        assert_eq!(rows[0][1], Value::I64(7));
    }

    async fn metrics_load(my: &My, s: &dyn Session) {
        let health = s.health().unwrap();
        for set in [MetricSet::Light, MetricSet::Heavy] {
            for m in health.metrics(set).await.unwrap() {
                assert!(
                    !matches!(m.status, MetricStatus::Failed { .. }),
                    "{} failed: {:?}",
                    m.id,
                    m.status
                );
            }
        }
        let low = my
            .flavor
            .driver()
            .connect(
                &my.config_for("low", None),
                ctx(Arc::new(MapSecrets::password("low-password"))),
            )
            .await
            .unwrap();
        let metrics = low
            .health()
            .unwrap()
            .metrics(MetricSet::Heavy)
            .await
            .unwrap();
        let active = metrics.iter().find(|m| m.id == "active_sessions").unwrap();
        assert_eq!(
            active.status,
            MetricStatus::NeedsGrant {
                grant: "PROCESS".into()
            }
        );
    }

    /// The key the grid identifies rows by: the primary key, else a unique key without NULLs.
    async fn rows_are_found_by_their_key(s: &dyn Session) {
        for sql in [
            "CREATE TABLE k_pk (a int, b int, c int NOT NULL UNIQUE, PRIMARY KEY (b, a))",
            "CREATE TABLE k_unique (x int NOT NULL, y int, UNIQUE (x))",
            "CREATE TABLE k_nullable (x int UNIQUE)",
            "CREATE TABLE k_none (x int)",
        ] {
            run(s, sql).await.unwrap();
        }
        let edit = s.edit().unwrap();
        let key = |table: &str| {
            let t = TableRef {
                schema: None,
                table: table.to_owned(),
            };
            async move { edit.row_key(&t).await.unwrap() }
        };
        assert_eq!(key("k_pk").await, vec!["b", "a"]);
        assert_eq!(key("k_unique").await, vec!["x"]);
        assert!(key("k_nullable").await.is_empty());
        assert!(key("k_none").await.is_empty());
        assert!(key("no_such_table").await.is_empty());
        run(s, "DROP TABLE k_pk, k_unique, k_nullable, k_none")
            .await
            .unwrap();
    }
}
