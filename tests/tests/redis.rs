//! The Redis driver against Redis 7 and the latest Valkey, each in a container.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod docker {
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::{Arc, Mutex};
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, GenericImage, ImageExt};
    use treasury_core::analyze::{CountPlan, StatementClass};
    use treasury_core::config::{ConnectionConfig, Endpoint, Engine, TlsMode};
    use treasury_core::driver::{ConnectContext, Driver, SecretSource, Session};
    use treasury_core::exec::{ExecEvent, ExecRequest};
    use treasury_core::schema::{ChildQuery, ObjectKind, ObjectPath};
    use treasury_core::value::{Blob, Value};
    use treasury_core::{ErrorKind, Result};
    use treasury_driver_redis::RedisDriver;
    use treasury_tests::MapSecrets;
    use treasury_tests::conformance::{Fixture, TypeCases, assert_all};
    use treasury_tests::docker::with_startup;

    const PASSWORD: &str = "conformance-password";

    struct Server {
        engine: Engine,
        container: ContainerAsync<GenericImage>,
        host: String,
        port: Mutex<u16>,
    }

    impl Server {
        async fn start(engine: Engine) -> Self {
            let (image, tag, binary) = match engine {
                Engine::Valkey => (
                    "valkey/valkey",
                    std::env::var("TREASURY_VALKEY_TAG").unwrap_or_else(|_| "9.2.1-alpine".into()),
                    "valkey-server",
                ),
                _ => (
                    "redis",
                    std::env::var("TREASURY_REDIS_TAG").unwrap_or_else(|_| "7.4.12-alpine".into()),
                    "redis-server",
                ),
            };
            let container = with_startup(
                GenericImage::new(image, &tag)
                    .with_exposed_port(6379.tcp())
                    .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections")),
            )
            .with_cmd([binary, "--requirepass", PASSWORD])
            .start()
            .await
            .expect("start the server");
            let host = container.get_host().await.unwrap().to_string();
            let port = container.get_host_port_ipv4(6379.tcp()).await.unwrap();
            let s = Self {
                engine,
                container,
                host,
                port: Mutex::new(port),
            };
            let session = s.session().await;
            run(
                session.as_ref(),
                "ACL SETUSER low on >low-password ~* +get +ping +info +client|id",
            )
            .await
            .unwrap();
            // A million entries, counting from 1, for the streaming check.
            for chunk in 0..100u64 {
                let values: Vec<String> = (chunk * 10_000 + 1..=(chunk + 1) * 10_000)
                    .map(|n| n.to_string())
                    .collect();
                run(
                    session.as_ref(),
                    &format!("RPUSH conf:rows {}", values.join(" ")),
                )
                .await
                .unwrap();
            }
            s
        }

        fn config_for(&self, user: &str) -> ConnectionConfig {
            let mut c = ConnectionConfig::new("redis", self.engine, &self.host).unwrap();
            c.endpoint = Endpoint::Host {
                host: self.host.clone(),
                port: *self.port.lock().unwrap(),
            };
            c.user = Some(user.into());
            c.tls.mode = TlsMode::Disable;
            c
        }

        async fn session(&self) -> Arc<dyn Session> {
            self.driver()
                .connect(&self.config(), ctx(self.secrets()))
                .await
                .unwrap()
        }
    }

    fn ctx(secrets: Arc<dyn SecretSource>) -> ConnectContext {
        ConnectContext {
            secrets,
            via: None,
            connect_timeout: std::time::Duration::from_secs(10),
            metadata_timeout: std::time::Duration::from_secs(10),
            read_only: false,
        }
    }

    async fn run(s: &dyn Session, text: &str) -> Result<Vec<Vec<Value>>> {
        let mut stream = s.execute().unwrap().execute(ExecRequest::new(text)).await?;
        let mut rows = Vec::new();
        while let Some(e) = stream.next().await {
            if let ExecEvent::Rows(r) = e? {
                rows.extend(r);
            }
        }
        Ok(rows)
    }

    async fn events(s: &dyn Session, text: &str) -> Vec<ExecEvent> {
        let mut stream = s
            .execute()
            .unwrap()
            .execute(ExecRequest::new(text))
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(e) = stream.next().await {
            out.push(e.unwrap());
        }
        out
    }

    #[async_trait]
    impl Fixture for Server {
        fn driver(&self) -> Arc<dyn Driver> {
            Arc::new(match self.engine {
                Engine::Valkey => RedisDriver::valkey(),
                _ => RedisDriver::redis(),
            })
        }
        fn config(&self) -> ConnectionConfig {
            self.config_for("default")
        }
        fn secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password(PASSWORD))
        }
        fn wrong_secrets(&self) -> Arc<dyn SecretSource> {
            Arc::new(MapSecrets::password("wrong"))
        }
        fn rows(&self, n: u64) -> String {
            format!("LRANGE conf:rows 0 {}", n.saturating_sub(1))
        }
        fn slow(&self, secs: u32) -> String {
            format!("BLPOP conf:never {secs}")
        }
        fn syntax_error(&self) -> (String, Option<u32>) {
            ("SET onlyonearg".into(), None)
        }
        fn write(&self) -> String {
            "SET conf:w 1".into()
        }
        fn types(&self) -> TypeCases {
            TypeCases {
                setup: vec!["SET conf:types:s héllo".into()],
                // RESP3 from Lua: an integer, text, a double, a boolean and bytes.
                query: r#"EVAL 'redis.setresp(3) return {{42, "h\195\169llo", {double=3.5}, true, string.char(255, 0)}}' 0"#.into(),
                expected: vec![
                    ("1".into(), Value::I64(42)),
                    ("2".into(), Value::Text("héllo".into())),
                    ("3".into(), Value::F64(3.5)),
                    ("4".into(), Value::Bool(true)),
                    ("5".into(), Value::Bytes(Blob::full(vec![255, 0]))),
                ],
            }
        }
        fn known_object(&self) -> ObjectPath {
            ObjectPath::root()
                .child(ObjectKind::Database, "0")
                .child(ObjectKind::KeyPrefix, "conf:")
                .child(ObjectKind::KeyPrefix, "conf:types:")
                .child(ObjectKind::Key, "conf:types:s")
        }
        fn unprivileged(&self) -> Option<(ConnectionConfig, Arc<dyn SecretSource>, String)> {
            Some((
                self.config_for("low"),
                Arc::new(MapSecrets::password("low-password")),
                "SET conf:x 1".into(),
            ))
        }
        async fn restart(&self) {
            self.container.stop().await.unwrap();
            self.container.start().await.unwrap();
            let port = self.container.get_host_port_ipv4(6379.tcp()).await.unwrap();
            *self.port.lock().unwrap() = port;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn redis_passes_conformance() {
        assert_all(&Server::start(Engine::Redis).await).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn valkey_passes_conformance() {
        assert_all(&Server::start(Engine::Valkey).await).await;
    }

    /// A master and a Sentinel watching it: the connection asks the Sentinel for the master.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sentinel_names_the_master() {
        let master = Server::start(Engine::Redis).await;
        let inside = master.container.get_bridge_ip_address().await.unwrap();
        let script = format!(
            "printf 'port 26379\\nsentinel monitor main {inside} 6379 1\\nsentinel auth-pass main {PASSWORD}\\n' > /tmp/s.conf && redis-sentinel /tmp/s.conf"
        );
        let sentinel = with_startup(
            GenericImage::new("redis", "7.4.12-alpine")
                .with_exposed_port(26379.tcp())
                .with_wait_for(WaitFor::message_on_stdout("+monitor master main")),
        )
        .with_cmd(["sh", "-c", script.as_str()])
        .start()
        .await
        .expect("start the sentinel");
        let host = sentinel.get_host().await.unwrap().to_string();
        let port = sentinel.get_host_port_ipv4(26379.tcp()).await.unwrap();
        let mut config = master.config();
        config.endpoint = Endpoint::Hosts {
            hosts: vec![(host, port)],
        };
        config.options.insert("mode".into(), "sentinel".into());
        config.options.insert("master_name".into(), "main".into());
        let s = master
            .driver()
            .connect(&config, ctx(master.secrets()))
            .await
            .unwrap();
        run(s.as_ref(), "SET through:sentinel yes").await.unwrap();
        let direct = master.session().await;
        assert_eq!(
            run(direct.as_ref(), "GET through:sentinel").await.unwrap(),
            [[Value::Text("yes".into())]]
        );
        let info = s.server_info().await.unwrap();
        assert!(
            info.details
                .contains(&("Mode".to_owned(), "sentinel".to_owned()))
        );
    }

    /// A cluster of one node holding every slot: commands go to it by key, and the key browser
    /// scans the primaries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cluster_routes_by_key() {
        let container = with_startup(
            GenericImage::new("redis", "7.4.12-alpine")
                .with_exposed_port(6379.tcp())
                .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections")),
        )
        .with_cmd([
            "redis-server",
            "--cluster-enabled",
            "yes",
            "--requirepass",
            PASSWORD,
            "--masterauth",
            PASSWORD,
        ])
        .start()
        .await
        .expect("start the node");
        let host = container.get_host().await.unwrap().to_string();
        let port = container.get_host_port_ipv4(6379.tcp()).await.unwrap();
        let inside = container.get_bridge_ip_address().await.unwrap();
        let mut config = ConnectionConfig::new("cluster", Engine::Redis, &host).unwrap();
        config.endpoint = Endpoint::Host { host, port };
        config.user = Some("default".into());
        config.tls.mode = TlsMode::Disable;
        let secrets: Arc<dyn SecretSource> = Arc::new(MapSecrets::password(PASSWORD));
        // The node gets every slot, and names itself by an address the tests reach.
        let node = RedisDriver::redis()
            .connect(&config, ctx(secrets.clone()))
            .await
            .unwrap();
        run(
            node.as_ref(),
            &format!("CONFIG SET cluster-announce-ip {inside}"),
        )
        .await
        .unwrap();
        run(node.as_ref(), "CLUSTER ADDSLOTSRANGE 0 16383")
            .await
            .unwrap();
        for _ in 0..60 {
            let info = run(node.as_ref(), "CLUSTER INFO").await.unwrap();
            if format!("{info:?}").contains("cluster_state:ok") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        config.options.insert("mode".into(), "cluster".into());
        let s = RedisDriver::redis()
            .connect(&config, ctx(secrets))
            .await
            .unwrap();
        assert_eq!(
            s.capabilities().cancel,
            treasury_core::driver::CancelMethod::ClientOnly
        );
        for k in ["a", "b", "c", "user:1", "user:2"] {
            run(s.as_ref(), &format!("SET {k} v-{k}")).await.unwrap();
        }
        assert_eq!(
            run(s.as_ref(), "GET user:2").await.unwrap(),
            [[Value::Text("v-user:2".into())]]
        );
        let tree = s.introspect().unwrap();
        let q = ChildQuery {
            filter: None,
            cursor: None,
            limit: 100,
        };
        let root = tree.children(&ObjectPath::root(), &q).await.unwrap();
        assert_eq!(root.nodes.len(), 1, "{root:?}");
        let keys = tree.children(&root.nodes[0].path, &q).await.unwrap();
        let names: Vec<&str> = keys.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["user:", "a", "b", "c"], "{keys:?}");
        assert_eq!(keys.nodes[1].detail.as_deref(), Some("string"));
        // A database other than 0 is refused: a cluster has none.
        let mut other = config.clone();
        other.database = Some("2".into());
        let e = RedisDriver::redis()
            .connect(&other, ctx(Arc::new(MapSecrets::password(PASSWORD))))
            .await
            .err()
            .unwrap();
        assert!(e.message.contains("only database 0"), "{e:?}");
    }

    struct NoHosts;

    #[async_trait]
    impl treasury_tunnel::HostKeyPrompt for NoHosts {
        async fn confirm_new(&self, _: &str, _: u16, _: &str, _: &str) -> bool {
            false
        }
    }

    /// The grid's path for keys: a hash, a list, a sorted set and a string edited through the
    /// runtime, as the app does it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn whole_keys_are_edited_from_the_grid() {
        use treasury_ipc::{CellEdit, RowEdits};
        let s = Server::start(Engine::Redis).await;
        let direct = s.session().await;
        for cmd in [
            "HSET user:9 name ada lang rust",
            "RPUSH q a b c",
            "ZADD z 1 a 2 b",
            "SET greeting hello EX 300",
        ] {
            run(direct.as_ref(), cmd).await.unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let rt = treasury_runtime::Runtime::new(
            Arc::new(
                treasury_store::Store::open_in_memory(treasury_vault::Sealer::new(
                    treasury_vault::random_key().unwrap(),
                ))
                .unwrap(),
            ),
            Arc::new(treasury_vault::MemoryStore::default()),
            vec![Arc::new(RedisDriver::redis())],
            treasury_tunnel::KnownHosts {
                app_file: dir.path().join("known_hosts"),
                user_file: None,
            },
            Arc::new(NoHosts),
            dir.path().to_owned(),
        );
        let config = s.config();
        rt.secrets_of(&config.id)
            .set(
                treasury_core::config::SecretSlot::Password,
                &secrecy::SecretString::from(PASSWORD),
            )
            .unwrap();
        rt.connect(&config).await.unwrap();
        let open = |stmt: &'static str| {
            let rt = &rt;
            let id = config.id.clone();
            async move {
                let p = rt.prepare(&id, stmt).await.unwrap();
                let rid = rt.run(&id, stmt, p.token, None, false).await.unwrap();
                rt.finish(rid).await.unwrap();
                rid
            }
        };
        let cell = |row: u64, column: usize, value: &str| CellEdit {
            row,
            column,
            value: Some(value.into()),
        };
        let edits = |cells: Vec<CellEdit>,
                     deletes: Vec<u64>,
                     inserts: Vec<Vec<(usize, Option<String>)>>| RowEdits {
            cells,
            deletes,
            inserts,
            expire: None,
        };

        // A hash: a value changed, a field removed, one added. The field names stay as they are.
        let rid = open("HGETALL user:9").await;
        let info = rt.edit_info(rid, 0).await.unwrap();
        assert_eq!(info.table.as_deref(), Some("the hash user:9"));
        assert_eq!(info.columns, [false, true]);
        let rows = rt.window(rid, 0, 0, 10).await.unwrap().rows;
        let at = |name: &str| {
            rows.iter()
                .position(|r| r[0] == Value::Text(name.into()))
                .unwrap() as u64
        };
        let e = edits(
            vec![cell(at("name"), 1, "Ada")],
            vec![at("lang")],
            vec![vec![(0, Some("city".into())), (1, Some("paris".into()))]],
        );
        let review = rt.edit_review(rid, 0, &e).await.unwrap();
        assert!(
            review.contains(&"HSET user:9 name Ada".to_owned()),
            "{review:?}"
        );
        rt.edit_apply(rid, 0, &e).await.unwrap();
        let after = run(direct.as_ref(), "HGETALL user:9").await.unwrap();
        let text = format!("{after:?}");
        assert!(
            text.contains("\"Ada\"") && text.contains("\"paris\"") && !text.contains("rust"),
            "{text}"
        );

        // A list: an entry changed by its position, one added; removing by position is refused.
        let rid = open("LRANGE q 0 -1").await;
        rt.edit_apply(
            rid,
            0,
            &edits(
                vec![cell(1, 0, "B")],
                vec![],
                vec![vec![(0, Some("d".into()))]],
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            run(direct.as_ref(), "LRANGE q 0 -1").await.unwrap(),
            [
                [Value::Text("a".into()), Value::I64(0)],
                [Value::Text("B".into()), Value::I64(1)],
                [Value::Text("c".into()), Value::I64(2)],
                [Value::Text("d".into()), Value::I64(3)],
            ]
        );
        let rid = open("LRANGE q 0 -1").await;
        let refused = rt
            .edit_review(rid, 0, &edits(vec![], vec![0], vec![]))
            .await
            .unwrap_err();
        assert!(refused.message.contains("LREM"), "{refused:?}");

        // A sorted set: a score changed.
        let rid = open("ZRANGE z 0 -1 WITHSCORES").await;
        rt.edit_apply(rid, 0, &edits(vec![cell(0, 1, "5")], vec![], vec![]))
            .await
            .unwrap();
        assert_eq!(
            run(direct.as_ref(), "ZRANGE z 0 -1").await.unwrap(),
            [[Value::Text("b".into())], [Value::Text("a".into())]]
        );

        // A string keeps its expiry.
        let rid = open("GET greeting").await;
        rt.edit_apply(rid, 0, &edits(vec![cell(0, 0, "hi")], vec![], vec![]))
            .await
            .unwrap();
        assert_eq!(
            run(direct.as_ref(), "GET greeting").await.unwrap(),
            [[Value::Text("hi".into())]]
        );
        let ttl = run(direct.as_ref(), "TTL greeting").await.unwrap();
        assert!(matches!(ttl[0][0], Value::I64(n) if n > 0), "{ttl:?}");

        // The key's expiry is shown with its rows and changes with them, in the same MULTI/EXEC.
        use treasury_ipc::Expiry;
        let ttl_of = |key: &'static str| {
            let direct = direct.as_ref();
            async move {
                match run(direct, &format!("TTL {key}")).await.unwrap()[0][0] {
                    Value::I64(n) => n,
                    ref other => panic!("{other:?}"),
                }
            }
        };
        let rid = open("GET greeting").await;
        let info = rt.edit_info(rid, 0).await.unwrap();
        assert!(
            matches!(info.expiry, Some(Expiry::After(n)) if n > 0 && n <= 300),
            "{info:?}"
        );
        let rid = open("HGETALL user:9").await;
        assert_eq!(
            rt.edit_info(rid, 0).await.unwrap().expiry,
            Some(Expiry::Never)
        );
        let e = RowEdits {
            cells: vec![],
            deletes: vec![],
            inserts: vec![vec![(0, Some("team".into())), (1, Some("core".into()))]],
            expire: Some(Expiry::After(3_600)),
        };
        assert_eq!(
            rt.edit_review(rid, 0, &e).await.unwrap(),
            ["HSET user:9 team core", "EXPIRE user:9 3600"]
        );
        assert_eq!(rt.edit_apply(rid, 0, &e).await.unwrap(), 1, "rows changed");
        let ttl = ttl_of("user:9").await;
        assert!((3_590..=3_600).contains(&ttl), "{ttl}");
        // An expiry alone is a change, and Never takes it away.
        let rid = open("HGETALL user:9").await;
        let persist = RowEdits {
            expire: Some(Expiry::Never),
            ..RowEdits::default()
        };
        assert_eq!(
            rt.edit_review(rid, 0, &persist).await.unwrap(),
            ["PERSIST user:9"]
        );
        rt.edit_apply(rid, 0, &persist).await.unwrap();
        assert_eq!(ttl_of("user:9").await, -1);
        let zero = RowEdits {
            expire: Some(Expiry::After(0)),
            ..RowEdits::default()
        };
        let refused = rt.edit_apply(rid, 0, &zero).await.unwrap_err();
        assert!(refused.message.contains("deletes the key"), "{refused:?}");
        assert_eq!(ttl_of("user:9").await, -1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn redis_specifics() {
        let s = Server::start(Engine::Redis).await;
        let session = s.session().await;
        let a = session.analyzer();
        // The server's command table decides.
        let class = |t: &str| a.classify(t)[0].class;
        assert_eq!(class("HGETALL h"), StatementClass::Read);
        assert_eq!(class("HSET h f v"), StatementClass::Write);
        assert_eq!(class("CONFIG GET maxmemory"), StatementClass::Admin);
        assert!(a.classify("FLUSHALL")[0].flags.production_forbidden);
        assert_eq!(a.count("DEL a b"), CountPlan::Query("EXISTS a b".into()));
        // Writes report what they changed; OK is just done.
        run(session.as_ref(), "SET k:1 a").await.unwrap();
        let ev = events(session.as_ref(), "DEL k:1 k:none").await;
        assert!(
            matches!(
                ev.last(),
                Some(ExecEvent::ResultEnd {
                    affected: Some(1),
                    ..
                })
            ),
            "{ev:?}"
        );
        // A hash reads as fields and values, in pages.
        let fields: Vec<String> = (0..2_500).map(|n| format!("f{n} {n}")).collect();
        run(
            session.as_ref(),
            &format!("HSET h:big {}", fields.join(" ")),
        )
        .await
        .unwrap();
        let rows = run(session.as_ref(), "HGETALL h:big").await.unwrap();
        assert_eq!(rows.len(), 2_500);
        assert_eq!(rows[0].len(), 2);
        // Sorted sets with scores.
        run(session.as_ref(), "ZADD z 1.5 a 2 b").await.unwrap();
        let rows = run(session.as_ref(), "ZRANGE z 0 -1 WITHSCORES")
            .await
            .unwrap();
        assert_eq!(
            rows,
            [
                [Value::Text("a".into()), Value::F64(1.5)],
                [Value::Text("b".into()), Value::F64(2.0)]
            ]
        );
        // Session commands are refused with the reason.
        let e = run(session.as_ref(), "MULTI").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        // The key browser: databases, then groups by ":".
        let tree = session.introspect().unwrap();
        let q = ChildQuery {
            filter: None,
            cursor: None,
            limit: 100,
        };
        let root = tree.children(&ObjectPath::root(), &q).await.unwrap();
        assert!(root.nodes.iter().any(|n| n.name == "db0"), "{root:?}");
        let db0 = ObjectPath::root().child(ObjectKind::Database, "0");
        let top = tree.children(&db0, &q).await.unwrap();
        let names: Vec<&str> = top.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.contains(&"conf:") && names.contains(&"h:"),
            "{names:?}"
        );
        let z = top.nodes.iter().find(|n| n.name == "z").unwrap();
        assert_eq!(z.detail.as_deref(), Some("zset"));
        let details = tree.details(&z.path).await.unwrap();
        assert_eq!(details.row_estimate, Some(2));
        // Another database is browsed on a connection of its own.
        run(session.as_ref(), "SELECT 3").await.unwrap();
        run(session.as_ref(), "SET in:three x").await.unwrap();
        let root = tree.children(&ObjectPath::root(), &q).await.unwrap();
        let three = root.nodes.iter().find(|n| n.name == "db3").unwrap();
        let keys = tree.children(&three.path, &q).await.unwrap();
        assert_eq!(keys.nodes[0].name, "in:");
    }
}
