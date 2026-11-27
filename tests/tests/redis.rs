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
