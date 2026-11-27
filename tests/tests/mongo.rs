//! The MongoDB driver against a real server in a container.
//!
//! MongoDB 8.3 refuses to start on Linux 6.19 and newer (SERVER-121912), so the default image
//! is the 8.0 line, which runs on every kernel CI and developers use.

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
    use treasury_core::exec::{ExecEvent, ExecRequest};
    use treasury_core::health::{MetricSet, MetricStatus};
    use treasury_core::plan::ExplainMode;
    use treasury_core::schema::{ChildQuery, ObjectKind, ObjectPath};
    use treasury_core::value::{Blob, List, ListKind, Timestamp, TimestampTz, Value};
    use treasury_core::{ErrorKind, Result};
    use treasury_driver_mongo::MongoDriver;
    use treasury_tests::MapSecrets;
    use treasury_tests::conformance::{Fixture, TypeCases, assert_all};
    use treasury_tests::docker::with_startup;

    const PASSWORD: &str = "conformance-password";

    struct Mongo {
        container: ContainerAsync<GenericImage>,
        host: String,
        port: Mutex<u16>,
    }

    impl Mongo {
        async fn start() -> Self {
            let tag = std::env::var("TREASURY_MONGO_TAG").unwrap_or_else(|_| "8.0.17".into());
            let container = with_startup(
                GenericImage::new("mongo", &tag)
                    .with_exposed_port(27017.tcp())
                    .with_wait_for(WaitFor::message_on_stdout("Waiting for connections")),
            )
            .with_env_var("MONGO_INITDB_ROOT_USERNAME", "root")
            .with_env_var("MONGO_INITDB_ROOT_PASSWORD", PASSWORD)
            .start()
            .await
            .expect("start mongo");
            let host = container.get_host().await.unwrap().to_string();
            let port = container.get_host_port_ipv4(27017.tcp()).await.unwrap();
            let m = Self {
                container,
                host,
                port: Mutex::new(port),
            };
            let s = m.wait_ready().await;
            for cmd in [
                r#"{"insert": "slow", "documents": [{"x": 1}]}"#,
                r#"{"insert": "secret", "documents": [{"x": 1}]}"#,
                r#"{"createUser": "low", "pwd": "low-password", "roles": [{"role": "read", "db": "other"}], "$db": "admin"}"#,
            ] {
                run(s.as_ref(), cmd)
                    .await
                    .unwrap_or_else(|e| panic!("{cmd}: {e}"));
            }
            m
        }

        async fn wait_ready(&self) -> Arc<dyn Session> {
            for _ in 0..120 {
                if let Ok(s) = MongoDriver
                    .connect(&self.config(), ctx(self.secrets()))
                    .await
                {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            panic!("mongo did not accept connections");
        }

        async fn session(&self) -> Arc<dyn Session> {
            MongoDriver
                .connect(&self.config(), ctx(self.secrets()))
                .await
                .unwrap()
        }

        fn config_for(&self, user: &str) -> ConnectionConfig {
            let mut c = ConnectionConfig::new("mongo", Engine::MongoDb, &self.host).unwrap();
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

    async fn run(s: &dyn Session, text: &str) -> Result<Vec<Vec<Value>>> {
        run_req(s, ExecRequest::new(text))
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
    impl Fixture for Mongo {
        fn driver(&self) -> Arc<dyn Driver> {
            Arc::new(MongoDriver)
        }
        fn config(&self) -> ConnectionConfig {
            self.config_for("root")
        }
        fn secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password(PASSWORD))
        }
        fn wrong_secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password("wrong"))
        }
        fn rows(&self, n: u64) -> String {
            format!(
                r#"{{"aggregate": 1, "pipeline": [
                    {{"$documents": [{{"a": {{"$range": [0, 1000]}}}}]}},
                    {{"$unwind": "$a"}},
                    {{"$set": {{"b": {{"$range": [0, 1000]}}}}}},
                    {{"$unwind": "$b"}},
                    {{"$project": {{"_id": 0, "n": {{"$toLong": {{"$add": [{{"$multiply": ["$a", 1000]}}, "$b", 1]}}}}}}}},
                    {{"$limit": {n}}}]}}"#
            )
        }
        fn slow(&self, secs: u32) -> String {
            format!(
                r#"{{"find": "slow", "filter": {{"$where": "sleep({}) || true"}}}}"#,
                secs * 1000
            )
        }
        fn syntax_error(&self) -> (String, Option<u32>) {
            (r#"{"find": }"#.into(), None)
        }
        fn write(&self) -> String {
            r#"{"insert": "ro_probe", "documents": [{"x": 1}]}"#.into()
        }
        fn types(&self) -> TypeCases {
            TypeCases {
                setup: vec![
                    r#"{"drop": "types"}"#.into(),
                    r#"{"insert": "types", "documents": [{"_id": 1, "b": true, "i": {"$numberInt": "-4"},
                       "l": {"$numberLong": "-8"}, "d": 2.25, "s": "héllo",
                       "oid": {"$oid": "65f0c0ffee0000000000abcd"},
                       "dt": {"$date": "2024-02-29T13:45:30.123Z"},
                       "bin": {"$binary": {"base64": "Af8=", "subType": "00"}},
                       "uuid": {"$binary": {"base64": "oO68mZwLTvi7bWu5vTgKEQ==", "subType": "04"}},
                       "arr": [1, "x"], "obj": {"k": 1}, "nul": null}]}"#
                        .into(),
                ],
                query: r#"{"find": "types", "filter": {}}"#.into(),
                expected: vec![
                    ("_id".into(), Value::I64(1)),
                    ("b".into(), Value::Bool(true)),
                    ("i".into(), Value::I64(-4)),
                    ("l".into(), Value::I64(-8)),
                    ("d".into(), Value::F64(2.25)),
                    ("s".into(), Value::Text("héllo".into())),
                    (
                        "oid".into(),
                        Value::ObjectId([
                            0x65, 0xf0, 0xc0, 0xff, 0xee, 0, 0, 0, 0, 0, 0xab, 0xcd,
                        ]),
                    ),
                    (
                        "dt".into(),
                        Value::TimestampTz(TimestampTz {
                            utc: Timestamp {
                                secs: 1_709_214_330,
                                nanos: 123_000_000,
                            },
                            offset_secs: 0,
                        }),
                    ),
                    ("bin".into(), Value::Bytes(Blob::full(vec![1, 255]))),
                    (
                        "uuid".into(),
                        Value::Uuid([
                            0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b,
                            0xb9, 0xbd, 0x38, 0x0a, 0x11,
                        ]),
                    ),
                    (
                        "arr".into(),
                        Value::List(List {
                            kind: ListKind::Array,
                            items: vec![Value::I64(1), Value::Text("x".into())],
                        }),
                    ),
                    ("obj".into(), Value::Document(vec![("k".into(), Value::I64(1))])),
                    ("nul".into(), Value::Null),
                ],
            }
        }
        fn known_object(&self) -> ObjectPath {
            ObjectPath::root()
                .child(ObjectKind::Database, "treasury")
                .child(ObjectKind::Group, "Collections")
                .child(ObjectKind::Collection, "types")
        }
        fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)> {
            Some((
                self.config_for("low"),
                Arc::new(MapSecrets::password("low-password")),
                r#"{"find": "secret"}"#.into(),
            ))
        }
        async fn restart(&self) {
            self.container.stop().await.unwrap();
            self.container.start().await.unwrap();
            let port = self
                .container
                .get_host_port_ipv4(27017.tcp())
                .await
                .unwrap();
            *self.port.lock().unwrap() = port;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mongo_passes_conformance() {
        assert_all(&Mongo::start().await).await;
    }

    struct NoHosts;

    #[async_trait]
    impl treasury_tunnel::HostKeyPrompt for NoHosts {
        async fn confirm_new(&self, _: &str, _: u16, _: &str, _: &str) -> bool {
            false
        }
    }

    /// The grid's path for documents: a find's result is edited and documents are added, through
    /// the runtime, as the app does it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn found_documents_are_edited_and_added_from_the_grid() {
        use treasury_ipc::{CellEdit, RowEdits};
        let m = Mongo::start().await;
        let s = m.session().await;
        run(
            s.as_ref(),
            r#"{"insert": "items", "documents": [{"_id": 1, "name": "a", "tags": []}]}"#,
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let rt = treasury_runtime::Runtime::new(
            Arc::new(
                treasury_store::Store::open_in_memory(treasury_vault::Sealer::new(
                    treasury_vault::random_key().unwrap(),
                ))
                .unwrap(),
            ),
            Arc::new(treasury_vault::MemoryStore::default()),
            vec![Arc::new(MongoDriver)],
            treasury_tunnel::KnownHosts {
                app_file: dir.path().join("known_hosts"),
                user_file: None,
            },
            Arc::new(NoHosts),
            dir.path().to_owned(),
        );
        let config = m.config();
        rt.secrets_of(&config.id)
            .set(
                treasury_core::config::SecretSlot::Password,
                &secrecy::SecretString::from(PASSWORD),
            )
            .unwrap();
        rt.connect(&config).await.unwrap();
        let find = r#"{"find": "items", "filter": {}, "$db": "treasury"}"#;
        let p = rt.prepare(&config.id, find).await.unwrap();
        let rid = rt
            .run(&config.id, find, p.token, None, false)
            .await
            .unwrap();
        rt.finish(rid).await.unwrap();
        let info = rt.edit_info(rid, 0).await.unwrap();
        assert_eq!(info.table.as_deref(), Some("treasury.items"), "{info:?}");
        let columns = rt.status(rid).unwrap().sets[0].columns.clone();
        let col = |n: &str| columns.iter().position(|c| c.name == n).unwrap();
        assert!(!info.columns[col("_id")], "a document's _id cannot change");
        assert!(info.columns[col("tags")]);
        let edits = RowEdits {
            cells: vec![CellEdit {
                row: 0,
                column: col("tags"),
                value: Some(r#"["x", {"y": 2}]"#.into()),
            }],
            deletes: vec![],
            inserts: vec![vec![(col("name"), Some("b".into()))]],
            expire: None,
        };
        let review = rt.edit_review(rid, 0, &edits).await.unwrap();
        assert_eq!(review.len(), 2, "{review:?}");
        assert_eq!(rt.edit_apply(rid, 0, &edits).await.unwrap(), 2);
        let docs = run(
            s.as_ref(),
            r#"{"find": "items", "filter": {}, "sort": {"name": 1}}"#,
        )
        .await
        .unwrap();
        let text = format!("{docs:?}");
        assert_eq!(docs.len(), 2, "{text}");
        assert!(text.contains("\"x\"") && text.contains("\"b\""), "{text}");
        assert!(matches!(docs[0][col("tags")], Value::List(_)), "{text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mongo_specifics() {
        let m = Mongo::start().await;
        let s = m.session().await;
        run(s.as_ref(), r#"{"insert": "orders", "documents": [{"_id": 1, "status": "open", "total": 5}, {"_id": 2, "status": "paid", "total": 9}, {"_id": 3, "status": "open", "total": 2, "note": "late"}]}"#)
            .await
            .unwrap();
        // One command at a time.
        assert_eq!(
            run(s.as_ref(), r#"{"ping": 1} {"drop": "orders"}"#)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        // Fields a later document adds are kept in the last column.
        let (rows, events) = run_req(
            s.as_ref(),
            ExecRequest {
                fetch_rows: 1,
                ..ExecRequest::new(r#"{"find": "orders", "filter": {}, "sort": {"_id": 1}}"#)
            },
        )
        .await
        .unwrap();
        let Some(ExecEvent::ResultStart(cols)) = events.first() else {
            panic!("{events:?}")
        };
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "status", "total", "…other fields"]);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[2][3],
            Value::Document(vec![("note".into(), Value::Text("late".into()))])
        );
        // Writes report what they changed.
        let (_, events) = run_req(
            s.as_ref(),
            ExecRequest::new(r#"{"update": "orders", "updates": [{"q": {"status": "open"}, "u": {"$set": {"seen": true}}, "multi": true}]}"#),
        )
        .await
        .unwrap();
        assert!(
            events.contains(&ExecEvent::ResultEnd {
                affected: Some(2),
                truncated: false
            }),
            "{events:?}"
        );
        // The count preview the guardrails run.
        let analyzer = s.analyzer();
        let treasury_core::analyze::CountPlan::Query(q) = analyzer
            .count(r#"{"delete": "orders", "deletes": [{"q": {"status": "open"}, "limit": 0}]}"#)
        else {
            panic!()
        };
        let counted = run(s.as_ref(), &q).await.unwrap();
        assert_eq!(counted[0][0], Value::I64(2));

        // Tree, details from a sample, indexes.
        run(s.as_ref(), r#"{"createIndexes": "orders", "indexes": [{"key": {"status": 1}, "name": "by_status"}]}"#)
            .await
            .unwrap();
        let tree = s.introspect().unwrap();
        let coll = ObjectPath::root()
            .child(ObjectKind::Database, "treasury")
            .child(ObjectKind::Collection, "orders");
        let details = tree.details(&coll).await.unwrap();
        assert!(
            details
                .columns
                .iter()
                .any(|c| c.name == "note" && c.nullable)
        );
        assert!(details.indexes.iter().any(|i| i.name == "by_status"));
        let indexes = tree
            .children(
                &coll.child(ObjectKind::Group, "Indexes"),
                &ChildQuery::default(),
            )
            .await
            .unwrap();
        assert!(indexes.nodes.iter().any(|n| n.name == "by_status"));

        // Plans.
        let plan = s
            .explain()
            .unwrap()
            .explain(
                r#"{"find": "orders", "filter": {"status": "open"}}"#,
                ExplainMode::Analyze,
            )
            .await
            .unwrap();
        assert!(!plan.root.operator.is_empty());

        // Edits on a standalone server run one by one.
        let edit = s.edit().unwrap();
        let change = |changes| Changeset {
            table: TableRef {
                schema: None,
                table: "orders".into(),
            },
            key_columns: vec!["_id".into()],
            changes,
            expire: None,
        };
        assert_eq!(
            edit.apply(&change(vec![
                RowChange::Update {
                    key: vec![Value::I64(2)],
                    set: vec![("status".into(), Value::Text("refunded".into()))]
                },
                RowChange::Delete {
                    key: vec![Value::I64(3)]
                },
            ]))
            .await
            .unwrap(),
            2
        );
        let left = run(
            s.as_ref(),
            r#"{"find": "orders", "filter": {}, "projection": {"status": 1}, "sort": {"_id": 1}}"#,
        )
        .await
        .unwrap();
        assert_eq!(left.len(), 2);
        assert_eq!(left[1][1], Value::Text("refunded".into()));
        assert_eq!(
            edit.apply(&change(vec![RowChange::Delete {
                key: vec![Value::I64(99)]
            }]))
            .await
            .unwrap_err()
            .kind,
            ErrorKind::Guard
        );

        for set in [MetricSet::Light, MetricSet::Heavy] {
            for metric in s.health().unwrap().metrics(set).await.unwrap() {
                assert!(
                    !matches!(metric.status, MetricStatus::Failed { .. }),
                    "{}: {:?}",
                    metric.id,
                    metric.status
                );
            }
        }
        let low = MongoDriver
            .connect(
                &m.config_for("low"),
                ctx(Arc::new(MapSecrets::password("low-password"))),
            )
            .await
            .unwrap();
        let metrics = low
            .health()
            .unwrap()
            .metrics(MetricSet::Light)
            .await
            .unwrap();
        assert!(
            metrics.iter().any(|m| m.status
                == MetricStatus::NeedsGrant {
                    grant: "clusterMonitor".into()
                }),
            "{metrics:?}"
        );
    }
}
