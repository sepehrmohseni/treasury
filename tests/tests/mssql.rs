//! The SQL Server driver against a real server in a container.
//!
//! The image only starts with `ACCEPT_EULA=Y`: the tests run SQL Server Developer Edition under
//! Microsoft's license for it, as the project's test plan requires.

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
    use treasury_core::value::{Blob, Date, Time, Timestamp, TimestampTz, Value};
    use treasury_core::{ErrorKind, Result};
    use treasury_driver_mssql::MsSqlDriver;
    use treasury_tests::MapSecrets;
    use treasury_tests::conformance::{Fixture, TypeCases, assert_all};
    use treasury_tests::docker::with_startup;

    const PASSWORD: &str = "Conformance-Passw0rd!";
    const LOW_PASSWORD: &str = "Ledger-Pa55word!";

    struct Ms {
        container: ContainerAsync<GenericImage>,
        host: String,
        port: Mutex<u16>,
    }

    impl Ms {
        async fn start() -> Self {
            let tag = std::env::var("TREASURY_MSSQL_TAG")
                .unwrap_or_else(|_| "2022-CU28-ubuntu-22.04".into());
            let container = with_startup(
                GenericImage::new("mcr.microsoft.com/mssql/server", &tag)
                    .with_exposed_port(1433.tcp())
                    .with_wait_for(WaitFor::message_on_stdout(
                        "SQL Server is now ready for client connections",
                    )),
            )
            .with_env_var("ACCEPT_EULA", "Y")
            .with_env_var("MSSQL_PID", "Developer")
            .with_env_var("MSSQL_SA_PASSWORD", PASSWORD)
            .start()
            .await
            .expect("start SQL Server");
            let host = container.get_host().await.unwrap().to_string();
            let port = container.get_host_port_ipv4(1433.tcp()).await.unwrap();
            let ms = Self {
                container,
                host,
                port: Mutex::new(port),
            };
            let master = ms.wait_ready(None).await;
            for sql in [
                "CREATE DATABASE treasury",
                &format!("CREATE LOGIN low WITH PASSWORD = '{LOW_PASSWORD}'"),
            ] {
                run(master.as_ref(), sql)
                    .await
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            let s = ms.session().await;
            for sql in [
                "CREATE USER low FOR LOGIN low",
                "CREATE TABLE secret_t (x int)",
                "CREATE TABLE ro_probe (x int)",
            ] {
                run(s.as_ref(), sql)
                    .await
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            ms
        }

        async fn wait_ready(&self, db: Option<&str>) -> Arc<dyn Session> {
            for _ in 0..240 {
                if let Ok(s) = MsSqlDriver
                    .connect(&self.config_for("sa", db), ctx(self.secrets()))
                    .await
                {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            panic!("SQL Server did not accept connections");
        }

        async fn session(&self) -> Arc<dyn Session> {
            MsSqlDriver
                .connect(&self.config(), ctx(self.secrets()))
                .await
                .unwrap()
        }

        fn config_for(&self, user: &str, db: Option<&str>) -> ConnectionConfig {
            let mut c = ConnectionConfig::new("ms", Engine::SqlServer, &self.host).unwrap();
            c.endpoint = Endpoint::Host {
                host: self.host.clone(),
                port: *self.port.lock().unwrap(),
            };
            c.user = Some(user.into());
            c.database = db.map(str::to_owned);
            // The container's certificate is self-signed.
            c.tls.mode = TlsMode::Require;
            c.tls.accept_invalid = true;
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
    impl Fixture for Ms {
        fn driver(&self) -> Arc<dyn Driver> {
            Arc::new(MsSqlDriver)
        }
        fn config(&self) -> ConnectionConfig {
            self.config_for("sa", Some("treasury"))
        }
        fn secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password(PASSWORD))
        }
        fn wrong_secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password("Wrong-Passw0rd!"))
        }
        fn rows(&self, n: u64) -> String {
            format!("SELECT CAST(value AS bigint) AS n FROM GENERATE_SERIES(1, {n}) ORDER BY value")
        }
        fn slow(&self, secs: u32) -> String {
            format!("WAITFOR DELAY '00:00:{secs:02}'")
        }
        fn syntax_error(&self) -> (String, Option<u32>) {
            ("SELECT 1 +".into(), Some(9))
        }
        fn write(&self) -> String {
            "INSERT INTO ro_probe VALUES (1)".into()
        }
        fn types(&self) -> TypeCases {
            type_cases()
        }
        fn known_object(&self) -> ObjectPath {
            ObjectPath::root()
                .child(ObjectKind::Database, "treasury")
                .child(ObjectKind::Schema, "dbo")
                .child(ObjectKind::Group, "Tables")
                .child(ObjectKind::Table, "types")
        }
        fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)> {
            Some((
                self.config_for("low", Some("treasury")),
                Arc::new(MapSecrets::password(LOW_PASSWORD)),
                "SELECT * FROM secret_t".into(),
            ))
        }
        async fn restart(&self) {
            self.container.stop().await.unwrap();
            self.container.start().await.unwrap();
            let port = self.container.get_host_port_ipv4(1433.tcp()).await.unwrap();
            *self.port.lock().unwrap() = port;
        }
    }

    /// 2024-02-29 13:45:30 as seconds since the epoch.
    const LEAP_DAY: i64 = 1_709_214_330;

    fn type_cases() -> TypeCases {
        TypeCases {
            setup: vec![
                "DROP TABLE IF EXISTS types".into(),
                "CREATE TABLE types (id int PRIMARY KEY, b bit, ti tinyint, si smallint, i int, \
                 bi bigint, r real, f float, d decimal(20,4), vc varchar(20), nvc nvarchar(20), \
                 c nchar(3), vb varbinary(4), u uniqueidentifier, dt date, tm time(1), dtm datetime, \
                 dt2 datetime2(6), dto datetimeoffset(0), sdt smalldatetime, x xml, nul int)"
                    .into(),
                "INSERT INTO types VALUES (1, 1, 255, -2, -4, -8, 1.5, 2.25, 12345.6789, 'hello', \
                 N'héllo', N'ab', 0x01FF, 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', '2024-02-29', \
                 '13:45:30.5', '2024-02-29 13:45:30.123', '2024-02-29 13:45:30.123456', \
                 '2024-02-29 13:45:30 +02:00', '2024-02-29 13:46:00', '<a>1</a>', NULL)"
                    .into(),
            ],
            query: "SELECT * FROM types".into(),
            expected: expected_types(),
        }
    }

    fn expected_types() -> Vec<(String, Value)> {
        vec![
            ("id".into(), Value::I64(1)),
            ("b".into(), Value::Bool(true)),
            ("ti".into(), Value::I64(255)),
            ("si".into(), Value::I64(-2)),
            ("i".into(), Value::I64(-4)),
            ("bi".into(), Value::I64(-8)),
            ("r".into(), Value::F32(1.5)),
            ("f".into(), Value::F64(2.25)),
            ("d".into(), Value::Decimal("12345.6789".into())),
            ("vc".into(), Value::Text("hello".into())),
            ("nvc".into(), Value::Text("héllo".into())),
            ("c".into(), Value::Text("ab ".into())),
            ("vb".into(), Value::Bytes(Blob::full(vec![1, 255]))),
            (
                "u".into(),
                Value::Uuid([
                    0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd,
                    0x38, 0x0a, 0x11,
                ]),
            ),
            ("dt".into(), Value::Date(Date { days: 19_782 })),
            (
                "tm".into(),
                Value::Time(Time {
                    nanos: (13 * 3600 + 45 * 60 + 30) * 1_000_000_000 + 500_000_000,
                }),
            ),
            // datetime counts in three-hundredths of a second: .123 is stored as 37/300.
            (
                "dtm".into(),
                Value::Timestamp(Timestamp {
                    secs: LEAP_DAY,
                    nanos: 123_333_333,
                }),
            ),
            (
                "dt2".into(),
                Value::Timestamp(Timestamp {
                    secs: LEAP_DAY,
                    nanos: 123_456_000,
                }),
            ),
            (
                "dto".into(),
                Value::TimestampTz(TimestampTz {
                    utc: Timestamp {
                        secs: LEAP_DAY - 7_200,
                        nanos: 0,
                    },
                    offset_secs: 7_200,
                }),
            ),
            (
                "sdt".into(),
                Value::Timestamp(Timestamp {
                    secs: LEAP_DAY + 30,
                    nanos: 0,
                }),
            ),
            ("x".into(), Value::Text("<a>1</a>".into())),
            ("nul".into(), Value::Null),
        ]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_server_passes_conformance() {
        assert_all(&Ms::start().await).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_server_specifics() {
        let ms = Ms::start().await;
        let s = ms.session().await;
        for sql in type_cases().setup {
            run(s.as_ref(), &sql).await.unwrap();
        }
        // A batch is the unit for T-SQL: the guardrails review it as a whole (its most
        // dangerous statement decides), and variables live across its statements. Text holding
        // several batches is refused.
        assert_eq!(
            run(s.as_ref(), "SELECT 1\nGO\nDROP TABLE types")
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            run(s.as_ref(), "DECLARE @x int = 5; SELECT @x")
                .await
                .unwrap(),
            vec![vec![Value::I64(5)]]
        );
        assert_eq!(
            run(s.as_ref(), "SELECT COUNT(*) FROM types").await.unwrap(),
            vec![vec![Value::I64(1)]]
        );
        let (rows, events) = run_req(
            s.as_ref(),
            ExecRequest::new("EXEC sp_executesql N'SELECT 1; SELECT 2'"),
        )
        .await
        .unwrap();
        assert_eq!(rows, vec![vec![Value::I64(1)], vec![Value::I64(2)]]);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ExecEvent::ResultStart(_)))
                .count(),
            2
        );
        // Affected counts and parameters.
        let (_, events) = run_req(
            s.as_ref(),
            ExecRequest::new("UPDATE types SET vc = vc WHERE id = 1"),
        )
        .await
        .unwrap();
        assert!(
            events.contains(&ExecEvent::ResultEnd {
                affected: Some(1),
                truncated: false
            }),
            "{events:?}"
        );
        let mut req = ExecRequest::new("SELECT COUNT(*) FROM types WHERE dt = @P1 AND nvc = @P2");
        req.params = vec![
            Param {
                name: None,
                value: Value::Date(Date { days: 19_782 }),
            },
            Param {
                name: None,
                value: Value::Text("héllo".into()),
            },
        ];
        assert_eq!(
            run_req(s.as_ref(), req).await.unwrap().0,
            vec![vec![Value::I64(1)]]
        );

        ddl_round_trips(s.as_ref()).await;
        tree_pages(s.as_ref()).await;
        edits(&ms, s.as_ref()).await;
        rows_are_found_by_their_key(s.as_ref()).await;

        let ex = s.explain().unwrap();
        let plan = ex
            .explain("SELECT * FROM types WHERE id = 1", ExplainMode::Estimate)
            .await
            .unwrap();
        assert!(!plan.root.operator.is_empty());
        let analyzed = ex
            .explain("SELECT COUNT(*) FROM types", ExplainMode::Analyze)
            .await
            .unwrap();
        // Some operators (Compute Scalar) carry no runtime counters; the plan as a whole does.
        fn measured(n: &treasury_core::plan::PlanNode) -> bool {
            n.actual_rows.is_some() || n.children.iter().any(measured)
        }
        assert!(measured(&analyzed.root), "{:?}", analyzed.root);

        run_req(
            s.as_ref(),
            ExecRequest::new("SELECT 1").with_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap();
        let (rows, _) = run_req(s.as_ref(), ExecRequest::new("WAITFOR DELAY '00:00:01.500'"))
            .await
            .unwrap();
        assert!(rows.is_empty());

        for set in [MetricSet::Light, MetricSet::Heavy] {
            for m in s.health().unwrap().metrics(set).await.unwrap() {
                assert!(
                    !matches!(m.status, MetricStatus::Failed { .. }),
                    "{} failed: {:?}",
                    m.id,
                    m.status
                );
            }
        }
        let low = MsSqlDriver
            .connect(
                &ms.config_for("low", Some("treasury")),
                ctx(Arc::new(MapSecrets::password(LOW_PASSWORD))),
            )
            .await
            .unwrap();
        let metrics = low
            .health()
            .unwrap()
            .metrics(MetricSet::Heavy)
            .await
            .unwrap();
        let active = metrics.iter().find(|m| m.id == "active_requests").unwrap();
        assert_eq!(
            active.status,
            MetricStatus::NeedsGrant {
                grant: "VIEW SERVER STATE".into()
            }
        );
    }

    async fn ddl_round_trips(s: &dyn Session) {
        for sql in [
            "CREATE TABLE customers (id int IDENTITY(1, 1) PRIMARY KEY, email nvarchar(100) NOT NULL UNIQUE)",
            "CREATE TABLE orders (id bigint PRIMARY KEY, customer_id int NOT NULL REFERENCES customers (id), \
             total decimal(12, 2) NOT NULL DEFAULT 0, placed date, CHECK (total < 1000000))",
            "CREATE INDEX orders_placed ON orders (placed DESC) INCLUDE (total) WHERE placed IS NOT NULL",
            "CREATE VIEW big_orders AS SELECT id, total FROM dbo.orders WHERE total > 100",
            "CREATE PROCEDURE order_count AS SELECT COUNT(*) FROM dbo.orders",
        ] {
            run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let tree = s.introspect().unwrap();
        let schema = ObjectPath::root()
            .child(ObjectKind::Database, "treasury")
            .child(ObjectKind::Schema, "dbo");
        let objects = [
            schema.child(ObjectKind::Table, "customers"),
            schema.child(ObjectKind::Table, "orders"),
            schema.child(ObjectKind::View, "big_orders"),
            schema.child(ObjectKind::Procedure, "order_count"),
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
            "DROP PROCEDURE order_count",
            "DROP VIEW big_orders",
            "DROP TABLE orders",
            "DROP TABLE customers",
        ] {
            run(s, sql).await.unwrap();
        }
        for text in &ddl {
            for st in s.analyzer().classify(text) {
                let one = &text[st.range.clone()];
                run(s, one)
                    .await
                    .unwrap_or_else(|e| panic!("replaying `{one}`: {e}"));
            }
        }
        for (o, before) in objects.iter().zip(&ddl) {
            assert_eq!(&tree.ddl(o).await.unwrap().unwrap(), before, "{o:?}");
        }
        assert!(ddl[0].contains("IDENTITY(1, 1)"), "{}", ddl[0]);
        assert!(ddl[1].contains("INCLUDE ([total])"), "{}", ddl[1]);
        let details = tree.details(&objects[1]).await.unwrap();
        assert_eq!(details.primary_key(), vec!["id"]);
        assert_eq!(details.foreign_keys[0].ref_table, "customers");
    }

    async fn tree_pages(s: &dyn Session) {
        for i in 0..25 {
            run(s, &format!("CREATE TABLE page_{i:02} (x int)"))
                .await
                .unwrap();
        }
        let tree = s.introspect().unwrap();
        let tables = ObjectPath::root()
            .child(ObjectKind::Database, "treasury")
            .child(ObjectKind::Schema, "dbo")
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
        let cols = tree
            .children(
                &ObjectPath::root()
                    .child(ObjectKind::Database, "treasury")
                    .child(ObjectKind::Schema, "dbo")
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
                schema: Some("dbo".into()),
                table: "edit_t".into(),
            },
            key_columns: vec!["id".into()],
            changes,
        }
    }

    async fn edits(ms: &Ms, s: &dyn Session) {
        run(
            s,
            "CREATE TABLE edit_t (id int PRIMARY KEY, note nvarchar(50), at date)",
        )
        .await
        .unwrap();
        run(
            s,
            "INSERT INTO edit_t VALUES (1, N'one', '2024-01-01'), (2, N'two', NULL)",
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
        assert_eq!(edit.apply(&ok).await.unwrap(), 3);
        assert_eq!(
            run(
                s,
                "SELECT id, note, CONVERT(varchar(10), at, 23) FROM edit_t ORDER BY id"
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
        // Inside the user's transaction the edit is undone by their rollback.
        let own = ms.session().await;
        run(own.as_ref(), "BEGIN TRANSACTION").await.unwrap();
        own.edit()
            .unwrap()
            .apply(&change(vec![RowChange::Update {
                key: vec![Value::I64(1)],
                set: vec![("note".into(), Value::Text("inside".into()))],
            }]))
            .await
            .unwrap();
        assert_eq!(
            run(own.as_ref(), "SELECT CAST(@@TRANCOUNT AS bigint)")
                .await
                .unwrap(),
            vec![vec![Value::I64(1)]],
            "the user's transaction must still be open"
        );
        run(own.as_ref(), "ROLLBACK TRANSACTION").await.unwrap();
        assert_eq!(
            run(s, "SELECT note FROM edit_t WHERE id = 1")
                .await
                .unwrap(),
            vec![vec![Value::Text("uno".into())]]
        );
    }

    /// The key the grid identifies rows by: the primary key, else a unique key without NULLs.
    async fn rows_are_found_by_their_key(s: &dyn Session) {
        for sql in [
            "CREATE TABLE k_pk (a int NOT NULL, b int NOT NULL, c int NOT NULL UNIQUE, \
             PRIMARY KEY (b, a))",
            "CREATE TABLE k_unique (x int NOT NULL, y int, UNIQUE (x))",
            "CREATE TABLE k_nullable (x int UNIQUE)",
            "CREATE TABLE k_filtered (x int NOT NULL)",
            "CREATE UNIQUE INDEX k_filtered_x ON k_filtered (x) WHERE x > 0",
            "CREATE TABLE k_none (x int)",
        ] {
            run(s, sql).await.unwrap();
        }
        let edit = s.edit().unwrap();
        let key = |schema: Option<&str>, table: &str| {
            let t = TableRef {
                schema: schema.map(str::to_owned),
                table: table.to_owned(),
            };
            async move { edit.row_key(&t).await.unwrap() }
        };
        assert_eq!(key(None, "k_pk").await, vec!["b", "a"]);
        assert_eq!(key(Some("dbo"), "k_pk").await, vec!["b", "a"]);
        assert_eq!(key(None, "k_unique").await, vec!["x"]);
        assert!(key(None, "k_nullable").await.is_empty());
        assert!(key(None, "k_filtered").await.is_empty());
        assert!(key(None, "k_none").await.is_empty());
        assert!(key(None, "no_such_table").await.is_empty());
        run(
            s,
            "DROP TABLE k_pk, k_unique, k_nullable, k_filtered, k_none",
        )
        .await
        .unwrap();
    }
}
