//! The PostgreSQL driver against a real server in a container.

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
    use treasury_core::value::{
        Blob, Date, Infinite, Interval, Json, List, ListKind, TemporalKind, Time, TimeTz,
        Timestamp, TimestampTz, Value,
    };
    use treasury_core::{ErrorKind, Result};
    use treasury_driver_postgres::PostgresDriver;
    use treasury_tests::MapSecrets;
    use treasury_tests::conformance::{Fixture, TypeCases, assert_all};
    use treasury_tests::docker::with_startup;

    /// The version CI tests; the nightly run adds the oldest supported one.
    const IMAGE_TAG: &str = "19.0-alpine";
    const PASSWORD: &str = "conformance-password";

    struct Pg {
        container: ContainerAsync<GenericImage>,
        host: String,
        port: Mutex<u16>,
    }

    impl Pg {
        async fn start() -> Self {
            let tag = std::env::var("TREASURY_POSTGRES_TAG").unwrap_or_else(|_| IMAGE_TAG.into());
            let container = with_startup(
                GenericImage::new("postgres", &tag)
                    .with_exposed_port(5432.tcp())
                    .with_wait_for(WaitFor::message_on_stdout(
                        "PostgreSQL init process complete",
                    )),
            )
            .with_env_var("POSTGRES_PASSWORD", PASSWORD)
            .with_env_var("POSTGRES_DB", "treasury")
            .start()
            .await
            .expect("start postgres");
            let host = container.get_host().await.unwrap().to_string();
            let port = container.get_host_port_ipv4(5432.tcp()).await.unwrap();
            let pg = Self {
                container,
                host,
                port: Mutex::new(port),
            };
            // The image restarts the server once after initialising; wait for the final one.
            pg.wait_ready().await;
            let s = pg.session().await;
            for sql in [
                "CREATE ROLE low LOGIN PASSWORD 'low-password'",
                "CREATE TABLE secret_t (x int)",
                "REVOKE ALL ON secret_t FROM PUBLIC",
            ] {
                run(s.as_ref(), sql).await.unwrap();
            }
            pg
        }

        async fn wait_ready(&self) {
            for _ in 0..120 {
                if PostgresDriver
                    .connect(&self.config(), ctx(self.secrets()))
                    .await
                    .is_ok()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            panic!("postgres did not accept connections");
        }

        async fn session(&self) -> Arc<dyn Session> {
            PostgresDriver
                .connect(&self.config(), ctx(self.secrets()))
                .await
                .unwrap()
        }

        fn config_for(&self, user: &str) -> ConnectionConfig {
            let mut c = ConnectionConfig::new("pg", Engine::Postgres, &self.host).unwrap();
            c.endpoint = Endpoint::Host {
                host: self.host.clone(),
                port: *self.port.lock().unwrap(),
            };
            c.user = Some(user.into());
            c.database = Some("treasury".into());
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

    /// Runs `sql` and returns the rows of its result.
    async fn run(s: &dyn Session, sql: &str) -> Result<Vec<Vec<Value>>> {
        run_req(s, ExecRequest::new(sql))
            .await
            .map(|(rows, _)| rows)
    }

    /// Runs a request and returns its rows and every event besides rows.
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
    impl Fixture for Pg {
        fn driver(&self) -> Arc<dyn Driver> {
            Arc::new(PostgresDriver)
        }
        fn config(&self) -> ConnectionConfig {
            self.config_for("postgres")
        }
        fn secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password(PASSWORD))
        }
        fn wrong_secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password("wrong"))
        }
        fn rows(&self, n: u64) -> String {
            format!("SELECT g::int8 FROM generate_series(1, {n}) AS g")
        }
        fn slow(&self, secs: u32) -> String {
            format!("SELECT pg_sleep({secs})")
        }
        fn syntax_error(&self) -> (String, Option<u32>) {
            ("SELEC 1".into(), Some(0))
        }
        fn write(&self) -> String {
            "CREATE TABLE ro_probe (x int)".into()
        }
        fn types(&self) -> TypeCases {
            type_cases()
        }
        fn known_object(&self) -> ObjectPath {
            ObjectPath::root()
                .child(ObjectKind::Schema, "public")
                .child(ObjectKind::Group, "Tables")
                .child(ObjectKind::Table, "types")
        }
        fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)> {
            Some((
                self.config_for("low"),
                Arc::new(MapSecrets::password("low-password")),
                "SELECT * FROM secret_t".into(),
            ))
        }
        async fn restart(&self) {
            self.container.stop().await.unwrap();
            self.container.start().await.unwrap();
            let port = self.container.get_host_port_ipv4(5432.tcp()).await.unwrap();
            *self.port.lock().unwrap() = port;
        }
    }

    /// 2024-02-29 13:45:30 as seconds since the epoch.
    const LEAP_DAY: i64 = 1_709_214_330;

    fn type_cases() -> TypeCases {
        TypeCases {
            setup: vec![
                "DROP TABLE IF EXISTS types".into(),
                "CREATE TABLE types (id int PRIMARY KEY, b boolean, i2 smallint, i4 integer, \
                 i8 bigint, f4 real, f8 double precision, n numeric(20,4), n_nan numeric, t text, \
                 vc varchar(10), c char(3), by bytea, d date, tm time, ttz timetz, ts timestamp, \
                 tstz timestamptz, iv interval, u uuid, j json, jb jsonb, ai int[], at text[], \
                 nul int, inf_d date, ninf_ts timestamp)"
                    .into(),
                "INSERT INTO types VALUES (1, true, -2, -4, -8, 1.5, 2.25, 12345.6789, 'NaN', \
                 'héllo', 'vc', 'ab', '\\x01ff', '2024-02-29', '13:45:30.5', '13:45:30+02', \
                 '2024-02-29 13:45:30.123456', '2024-02-29 13:45:30+02', \
                 '1 year 2 mons 3 days 04:05:06.5', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', \
                 '{\"b\": 1, \"a\": [1, 2]}', '{\"b\": 1, \"a\": 2}', '{1,NULL,3}', \
                 '{\"a b\",\"c,d\"}', NULL, 'infinity', '-infinity')"
                    .into(),
            ],
            query: "SELECT * FROM types".into(),
            expected: expected_types(),
        }
    }

    fn expected_types() -> Vec<(String, Value)> {
        let secs_of_day = 13 * 3600 + 45 * 60 + 30;
        vec![
            ("id".into(), Value::I64(1)),
            ("b".into(), Value::Bool(true)),
            ("i2".into(), Value::I64(-2)),
            ("i4".into(), Value::I64(-4)),
            ("i8".into(), Value::I64(-8)),
            ("f4".into(), Value::F32(1.5)),
            ("f8".into(), Value::F64(2.25)),
            ("n".into(), Value::Decimal("12345.6789".into())),
            ("n_nan".into(), Value::Decimal("NaN".into())),
            ("t".into(), Value::Text("héllo".into())),
            ("vc".into(), Value::Text("vc".into())),
            ("c".into(), Value::Text("ab ".into())),
            ("by".into(), Value::Bytes(Blob::full(vec![1, 255]))),
            ("d".into(), Value::Date(Date { days: 19_782 })),
            (
                "tm".into(),
                Value::Time(Time {
                    nanos: secs_of_day * 1_000_000_000 + 500_000_000,
                }),
            ),
            (
                "ttz".into(),
                Value::TimeTz(TimeTz {
                    time: Time {
                        nanos: secs_of_day * 1_000_000_000,
                    },
                    offset_secs: 7_200,
                }),
            ),
            (
                "ts".into(),
                Value::Timestamp(Timestamp {
                    secs: LEAP_DAY,
                    nanos: 123_456_000,
                }),
            ),
            (
                "tstz".into(),
                Value::TimestampTz(TimestampTz {
                    utc: Timestamp {
                        secs: LEAP_DAY - 7_200,
                        nanos: 0,
                    },
                    offset_secs: 0,
                }),
            ),
            (
                "iv".into(),
                Value::Interval(Interval {
                    months: 14,
                    days: 3,
                    nanos: (4 * 3600 + 5 * 60 + 6) * 1_000_000_000 + 500_000_000,
                }),
            ),
            (
                "u".into(),
                Value::Uuid([
                    0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd,
                    0x38, 0x0a, 0x11,
                ]),
            ),
            (
                "j".into(),
                Value::Json(Json {
                    text: "{\"b\": 1, \"a\": [1, 2]}".into(),
                    binary: false,
                }),
            ),
            (
                "jb".into(),
                Value::Json(Json {
                    text: "{\"a\": 2, \"b\": 1}".into(),
                    binary: true,
                }),
            ),
            (
                "ai".into(),
                Value::List(List {
                    kind: ListKind::Array,
                    items: vec![Value::I64(1), Value::Null, Value::I64(3)],
                }),
            ),
            (
                "at".into(),
                Value::List(List {
                    kind: ListKind::Array,
                    items: vec![Value::Text("a b".into()), Value::Text("c,d".into())],
                }),
            ),
            ("nul".into(), Value::Null),
            (
                "inf_d".into(),
                Value::Infinite(Infinite {
                    kind: TemporalKind::Date,
                    negative: false,
                }),
            ),
            (
                "ninf_ts".into(),
                Value::Infinite(Infinite {
                    kind: TemporalKind::Timestamp,
                    negative: true,
                }),
            ),
        ]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn postgres_passes_conformance() {
        let pg = Pg::start().await;
        assert_all(&pg).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn postgres_specifics() {
        let pg = Pg::start().await;
        let s = pg.session().await;
        for sql in type_cases().setup {
            run(s.as_ref(), &sql).await.unwrap();
        }
        binary_results_match_text_results(s.as_ref()).await;
        statements_run_one_at_a_time(s.as_ref()).await;
        notices_arrive_with_results(s.as_ref()).await;
        ddl_round_trips(s.as_ref()).await;
        tree_pages_and_filters(s.as_ref()).await;
        edits_are_atomic(s.as_ref()).await;
        edits_join_an_open_transaction(&pg).await;
        plans_are_trees(s.as_ref()).await;
        a_finished_statement_leaves_no_timer(s.as_ref()).await;
        metrics_load(&pg, s.as_ref()).await;
    }

    /// Statements with parameters are read in the binary format; they must decode to the same
    /// values as the text format.
    async fn binary_results_match_text_results(s: &dyn Session) {
        let text = run(s, "SELECT * FROM types").await.unwrap();
        let mut req = ExecRequest::new("SELECT * FROM types WHERE id = $1");
        req.params = vec![Param {
            name: None,
            value: Value::I64(1),
        }];
        let (binary, _) = run_req(s, req).await.unwrap();
        let names: Vec<String> = expected_types().into_iter().map(|(n, _)| n).collect();
        let mut wrong = Vec::new();
        for (i, (t, b)) in text[0].iter().zip(&binary[0]).enumerate() {
            if t != b {
                wrong.push(format!("{}: text {t:?}, binary {b:?}", names[i]));
            }
        }
        assert!(
            wrong.is_empty(),
            "binary and text decoding differ:\n{}",
            wrong.join("\n")
        );

        // A value typed in the UI as text binds to any column type.
        let mut req = ExecRequest::new("SELECT count(*) FROM types WHERE iv = $1 AND u = $2");
        req.params = vec![
            Param {
                name: None,
                value: Value::Text("P1Y2M3DT4H5M6.5S".into()),
            },
            Param {
                name: None,
                value: Value::Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
            },
        ];
        let (rows, _) = run_req(s, req).await.unwrap();
        assert_eq!(rows, vec![vec![Value::I64(1)]]);
    }

    async fn statements_run_one_at_a_time(s: &dyn Session) {
        let err = run(s, "SELECT 1; DROP TABLE types").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported);
        assert_eq!(
            run(s, "SELECT count(*) FROM types").await.unwrap(),
            vec![vec![Value::I64(1)]],
            "the second statement must not have run"
        );
        let err = run(s, "COPY types TO STDOUT").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported);
        // Affected counts for writes.
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

    async fn notices_arrive_with_results(s: &dyn Session) {
        let (_, events) = run_req(
            s,
            ExecRequest::new("DO $$ BEGIN RAISE NOTICE 'hello from the server'; END $$"),
        )
        .await
        .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExecEvent::Notice(n) if n.message == "hello from the server")),
            "{events:?}"
        );
    }

    /// DDL read back from the catalogs recreates the same objects.
    async fn ddl_round_trips(s: &dyn Session) {
        for sql in [
            "CREATE SCHEMA shop",
            "CREATE TYPE shop.mood AS ENUM ('sad', 'ok', 'it''s fine')",
            "CREATE DOMAIN shop.positive AS int CHECK (VALUE > 0)",
            "CREATE TYPE shop.pair AS (a int, b text)",
            "CREATE TABLE shop.customers (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
             email text NOT NULL UNIQUE, mood shop.mood DEFAULT 'ok')",
            "CREATE TABLE shop.orders (id bigserial PRIMARY KEY, \
             customer_id int NOT NULL REFERENCES shop.customers (id), \
             qty shop.positive, total numeric(12,2) GENERATED ALWAYS AS (qty * 2.5) STORED, \
             placed date NOT NULL DEFAULT current_date, CHECK (total < 1000000))",
            "CREATE INDEX orders_placed ON shop.orders (placed DESC) WHERE qty > 1",
            "COMMENT ON TABLE shop.orders IS 'What people bought'",
            "COMMENT ON COLUMN shop.orders.qty IS 'How many'",
            "CREATE VIEW shop.big_orders AS SELECT id, total FROM shop.orders WHERE total > 100",
            "CREATE MATERIALIZED VIEW shop.daily AS SELECT placed, count(*) AS n FROM shop.orders \
             GROUP BY placed",
            "CREATE FUNCTION shop.double(x int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT x * 2 $$",
            "CREATE SEQUENCE shop.tickets START 100 INCREMENT 5",
            "CREATE TABLE shop.events (at date NOT NULL, kind text) PARTITION BY RANGE (at)",
            "CREATE TABLE shop.events_2026 PARTITION OF shop.events \
             FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
        ] {
            run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let tree = s.introspect().unwrap();
        let schema = ObjectPath::root().child(ObjectKind::Schema, "shop");
        let objects = [
            schema.child(ObjectKind::Type, "mood"),
            schema.child(ObjectKind::Type, "positive"),
            schema.child(ObjectKind::Type, "pair"),
            schema.child(ObjectKind::Table, "customers"),
            schema.child(ObjectKind::Table, "orders"),
            schema.child(ObjectKind::View, "big_orders"),
            schema.child(ObjectKind::MaterializedView, "daily"),
            schema.child(ObjectKind::Function, "double(x integer)"),
            schema.child(ObjectKind::Sequence, "tickets"),
            schema.child(ObjectKind::Table, "events"),
            schema.child(ObjectKind::Table, "events_2026"),
        ];
        let mut ddl = Vec::new();
        for o in &objects {
            let text = tree
                .ddl(o)
                .await
                .unwrap_or_else(|e| panic!("{o:?}: {e}"))
                .unwrap_or_else(|| panic!("{o:?}: no DDL"));
            ddl.push(text);
        }
        // Recreate everything from the DDL in a fresh schema and read the DDL again.
        run(s, "DROP SCHEMA shop CASCADE").await.unwrap();
        run(s, "CREATE SCHEMA shop").await.unwrap();
        for text in &ddl {
            for st in s.analyzer().classify(text) {
                let one = &text[st.range.clone()];
                run(s, one)
                    .await
                    .unwrap_or_else(|e| panic!("replaying `{one}`: {e}"));
            }
        }
        for (o, before) in objects.iter().zip(&ddl) {
            let after = tree.ddl(o).await.unwrap().unwrap();
            assert_eq!(&after, before, "DDL of {o:?} changed after a round trip");
        }
        assert!(
            ddl[4].contains("COMMENT ON TABLE shop.orders IS 'What people bought';"),
            "{}",
            ddl[4]
        );
        assert!(ddl[4].contains("id bigserial NOT NULL"), "{}", ddl[4]);
        assert!(ddl[4].contains(") STORED"), "{}", ddl[4]);
        assert!(ddl[10].contains("PARTITION OF shop.events"), "{}", ddl[10]);
    }

    async fn tree_pages_and_filters(s: &dyn Session) {
        for i in 0..25 {
            run(s, &format!("CREATE TABLE public.page_{i:02} (x int)"))
                .await
                .unwrap();
        }
        let tree = s.introspect().unwrap();
        let tables = ObjectPath::root()
            .child(ObjectKind::Schema, "public")
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
        // `_` in a filter is a character, not a wildcard.
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
        assert!(none.nodes.is_empty(), "{:?}", none.nodes);
        // Columns come in table order.
        let cols = tree
            .children(
                &ObjectPath::root()
                    .child(ObjectKind::Schema, "public")
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
        assert_eq!(names, ["id", "b", "i2"]);
        let details = tree
            .details(
                &ObjectPath::root()
                    .child(ObjectKind::Schema, "public")
                    .child(ObjectKind::Table, "types"),
            )
            .await
            .unwrap();
        assert_eq!(details.primary_key(), vec!["id"]);
        assert_eq!(details.columns.len(), expected_types().len());
    }

    fn orders_change(changes: Vec<RowChange>) -> Changeset {
        Changeset {
            table: TableRef {
                schema: Some("public".into()),
                table: "edit_t".into(),
            },
            key_columns: vec!["id".into()],
            changes,
        }
    }

    async fn edits_are_atomic(s: &dyn Session) {
        run(
            s,
            "CREATE TABLE edit_t (id int PRIMARY KEY, note text, at date)",
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
        let ok = orders_change(vec![
            RowChange::Update {
                key: vec![Value::I64(1)],
                set: vec![
                    ("note".into(), Value::Text("uno".into())),
                    // Typed in the grid as text, stored as a date.
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
            run(s, "SELECT id, note, at::text FROM edit_t ORDER BY id")
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
        // A row that is gone makes the whole changeset fail, and nothing is kept.
        let stale = orders_change(vec![
            RowChange::Update {
                key: vec![Value::I64(3)],
                set: vec![("note".into(), Value::Text("changed".into()))],
            },
            RowChange::Delete {
                key: vec![Value::I64(99)],
            },
        ]);
        let err = edit.apply(&stale).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Guard);
        assert_eq!(
            run(s, "SELECT note FROM edit_t WHERE id = 3")
                .await
                .unwrap(),
            vec![vec![Value::Text("three".into())]]
        );
        let review = edit.render(&ok).unwrap();
        assert_eq!(
            review[0],
            "UPDATE \"public\".\"edit_t\" SET \"note\" = 'uno', \"at\" = '2025-12-31' WHERE \"id\" = 1;"
        );
    }

    async fn edits_join_an_open_transaction(pg: &Pg) {
        let s = pg.session().await;
        run(s.as_ref(), "BEGIN").await.unwrap();
        let change = orders_change(vec![RowChange::Update {
            key: vec![Value::I64(1)],
            set: vec![("note".into(), Value::Text("inside".into()))],
        }]);
        s.edit().unwrap().apply(&change).await.unwrap();
        // Another session does not see it until the user's transaction commits.
        let other = pg.session().await;
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

    /// The time limit of a statement that finished must not cancel the next one.
    async fn a_finished_statement_leaves_no_timer(s: &dyn Session) {
        run_req(
            s,
            ExecRequest::new("SELECT 1").with_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap();
        let (rows, _) = run_req(s, ExecRequest::new("SELECT pg_sleep(1.5), 7"))
            .await
            .unwrap();
        assert_eq!(rows[0][1], Value::I64(7));
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
        assert!(plan.root.cost.is_some());
        assert!(plan.raw.contains("\"Plan\""));
        let analyzed = ex
            .explain(
                "SELECT count(*) FROM generate_series(1, 1000)",
                ExplainMode::Analyze,
            )
            .await
            .unwrap();
        assert!(analyzed.root.actual_rows.is_some(), "{:?}", analyzed.root);
        // Text after a semicolon never runs with the plan.
        assert!(
            ex.explain("SELECT 1; DROP TABLE types", ExplainMode::Estimate)
                .await
                .is_err()
        );
        assert_eq!(
            run(s, "SELECT count(*) FROM types").await.unwrap(),
            vec![vec![Value::I64(1)]]
        );
    }

    async fn metrics_load(pg: &Pg, s: &dyn Session) {
        let health = s.health().unwrap();
        for set in [MetricSet::Light, MetricSet::Heavy] {
            let metrics = health.metrics(set).await.unwrap();
            assert!(!metrics.is_empty());
            for m in &metrics {
                assert!(
                    !matches!(m.status, MetricStatus::Failed { .. }),
                    "{} failed: {:?}",
                    m.id,
                    m.status
                );
            }
        }
        // A user without pg_read_all_stats is told which grant would show other sessions.
        let low = PostgresDriver
            .connect(
                &pg.config_for("low"),
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
                grant: "pg_read_all_stats".into()
            }
        );
    }
}
