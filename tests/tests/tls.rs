//! TLS against real servers with a generated CA: the server's certificate is checked against the
//! configured name (not the address dialled), a custom CA is the only one trusted, and client
//! certificates authenticate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod docker {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, GenericImage, ImageExt};
    use treasury_core::config::{ConnectionConfig, Endpoint, Engine, TlsMode};
    use treasury_core::driver::{ConnectContext, Driver, Session};
    use treasury_core::{ErrorKind, Result};
    use treasury_driver_mysql::MySqlDriver;
    use treasury_driver_postgres::PostgresDriver;
    use treasury_tests::MapSecrets;
    use treasury_tests::docker::with_startup;

    const PASSWORD: &str = "tls-password";
    /// The name in the server certificate; the tests dial an address that is not this name.
    const SERVER_NAME: &str = "db.test";

    struct Pki {
        ca: String,
        server_cert: String,
        server_key: String,
        client_cert: String,
        client_key: String,
    }

    fn pki(client_cn: &str) -> Pki {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "Treasury test CA");
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);

        let server_key = KeyPair::generate().unwrap();
        let mut server = CertificateParams::new(vec![SERVER_NAME.into()]).unwrap();
        server
            .distinguished_name
            .push(DnType::CommonName, SERVER_NAME);
        let server_cert = server.signed_by(&server_key, &issuer).unwrap();

        let client_key = KeyPair::generate().unwrap();
        let mut client = CertificateParams::new(Vec::<String>::new()).unwrap();
        client
            .distinguished_name
            .push(DnType::CommonName, client_cn);
        let client_cert = client.signed_by(&client_key, &issuer).unwrap();

        Pki {
            ca: ca_cert.pem(),
            server_cert: server_cert.pem(),
            server_key: server_key.serialize_pem(),
            client_cert: client_cert.pem(),
            client_key: client_key.serialize_pem(),
        }
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn ctx() -> ConnectContext {
        ConnectContext {
            secrets: Arc::new(MapSecrets::password(PASSWORD)),
            via: None,
            connect_timeout: Duration::from_secs(10),
            metadata_timeout: Duration::from_secs(10),
            read_only: false,
        }
    }

    async fn connect(driver: &dyn Driver, c: &ConnectionConfig) -> Result<Arc<dyn Session>> {
        driver.connect(c, ctx()).await
    }

    /// The certificates go in through environment variables; a shell writes them where the
    /// server expects them, with the ownership it requires, before starting it.
    fn with_pki(
        image: GenericImage,
        pki: &Pki,
        script: &str,
    ) -> testcontainers::ContainerRequest<GenericImage> {
        with_startup(image.with_entrypoint("sh"))
            .with_cmd(["-c", script])
            .with_env_var("TLS_CA", pki.ca.clone())
            .with_env_var("TLS_CERT", pki.server_cert.clone())
            .with_env_var("TLS_KEY", pki.server_key.clone())
    }

    async fn host_port(c: &ContainerAsync<GenericImage>, port: u16) -> (String, u16) {
        (
            c.get_host().await.unwrap().to_string(),
            c.get_host_port_ipv4(port.tcp()).await.unwrap(),
        )
    }

    struct Case {
        label: &'static str,
        config: ConnectionConfig,
        works: bool,
    }

    /// The same configuration varied: what must connect and what must be refused.
    fn cases(base: &ConnectionConfig, dir: &Path, pki: &Pki, client_cert: bool) -> Vec<Case> {
        let ca = write(dir, "ca.pem", &pki.ca);
        let cert = write(dir, "client.pem", &pki.client_cert);
        let key = write(dir, "client.key", &pki.client_key);
        let mut verified = base.clone();
        verified.tls.mode = TlsMode::Require;
        verified.tls.ca_file = Some(ca);
        verified.tls.server_name = Some(SERVER_NAME.into());
        if client_cert {
            verified.tls.client_cert = Some(cert);
            verified.tls.client_key = Some(key);
        }
        let mut system_roots = verified.clone();
        system_roots.tls.ca_file = None;
        let mut wrong_name = verified.clone();
        wrong_name.tls.server_name = Some("other.test".into());
        let mut accept_invalid = system_roots.clone();
        accept_invalid.tls.accept_invalid = true;
        let mut plain = verified.clone();
        plain.tls.mode = TlsMode::Disable;
        let mut out = vec![
            Case {
                label: "verified with the custom CA",
                config: verified.clone(),
                works: true,
            },
            Case {
                label: "system roots do not know the CA",
                config: system_roots,
                works: false,
            },
            Case {
                label: "the name does not match",
                config: wrong_name,
                works: false,
            },
            Case {
                label: "invalid certificates accepted on request",
                config: accept_invalid,
                works: true,
            },
            Case {
                label: "plain TCP is refused by the server",
                config: plain,
                works: false,
            },
        ];
        if client_cert {
            let mut no_cert = verified;
            no_cert.tls.client_cert = None;
            no_cert.tls.client_key = None;
            out.push(Case {
                label: "a client certificate is required",
                config: no_cert,
                works: false,
            });
        }
        out
    }

    async fn check(driver: &dyn Driver, cases: Vec<Case>) {
        let mut wrong = Vec::new();
        for case in cases {
            match connect(driver, &case.config).await {
                Ok(s) => {
                    s.ping().await.unwrap();
                    if !case.works {
                        wrong.push(format!("{}: connected", case.label));
                    }
                }
                Err(e) => {
                    if case.works {
                        wrong.push(format!("{}: {e}", case.label));
                    } else if (case.label.contains("roots") || case.label.contains("name"))
                        && e.kind != ErrorKind::Tls
                    {
                        wrong.push(format!(
                            "{}: expected a TLS error, got {:?}",
                            case.label, e.kind
                        ));
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_verifies_certificates_both_ways() {
        let pki = pki("postgres");
        let script = "set -e; d=/var/lib/postgresql/tls; mkdir -p $d; \
            printf '%s' \"$TLS_CA\" > $d/ca.pem; printf '%s' \"$TLS_CERT\" > $d/server.pem; \
            printf '%s' \"$TLS_KEY\" > $d/server.key; \
            printf 'local all all trust\\nhostssl all all all scram-sha-256 clientcert=verify-ca\\n' > $d/hba.conf; \
            chown -R postgres:postgres $d; chmod 600 $d/server.key; \
            exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=$d/server.pem \
              -c ssl_key_file=$d/server.key -c ssl_ca_file=$d/ca.pem -c hba_file=$d/hba.conf";
        let c = with_pki(
            GenericImage::new("postgres", "19.0-alpine")
                .with_exposed_port(5432.tcp())
                .with_wait_for(WaitFor::message_on_stdout(
                    "PostgreSQL init process complete",
                )),
            &pki,
            script,
        )
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .start()
        .await
        .unwrap();
        let (host, port) = host_port(&c, 5432).await;
        let mut base = ConnectionConfig::new("pg", Engine::Postgres, &host).unwrap();
        base.endpoint = Endpoint::Host { host, port };
        base.user = Some("postgres".into());
        let dir = tempfile::tempdir().unwrap();
        let cases = cases(&base, dir.path(), &pki, true);
        let first = cases[0].config.clone();
        for _ in 0..60 {
            if connect(&PostgresDriver, &first).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        check(&PostgresDriver, cases).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mysql_verifies_certificates_both_ways() {
        let pki = pki("cert_user");
        let script = "set -e; d=/etc/mysql/tls; mkdir -p $d; \
            printf '%s' \"$TLS_CA\" > $d/ca.pem; printf '%s' \"$TLS_CERT\" > $d/server.pem; \
            printf '%s' \"$TLS_KEY\" > $d/server.key; chown -R mysql:mysql $d; chmod 600 $d/server.key; \
            exec docker-entrypoint.sh mysqld --ssl-ca=$d/ca.pem --ssl-cert=$d/server.pem \
              --ssl-key=$d/server.key --require-secure-transport=ON";
        let c = with_pki(
            GenericImage::new("mysql", "9.7.0")
                .with_exposed_port(3306.tcp())
                .with_wait_for(WaitFor::message_on_stderr("port: 3306")),
            &pki,
            script,
        )
        .with_env_var("MYSQL_ROOT_PASSWORD", PASSWORD)
        .start()
        .await
        .unwrap();
        let (host, port) = host_port(&c, 3306).await;
        let mut base = ConnectionConfig::new("my", Engine::MySql, &host).unwrap();
        base.endpoint = Endpoint::Host { host, port };
        base.user = Some("root".into());
        let dir = tempfile::tempdir().unwrap();
        let root_cases = cases(&base, dir.path(), &pki, false);
        let admin = root_cases[0].config.clone();
        let mut session = None;
        for _ in 0..120 {
            if let Ok(s) = connect(&MySqlDriver::mysql(), &admin).await {
                session = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let s = session.expect("mysql never accepted the verified connection");
        // An account that must present a certificate issued by the CA.
        let mut stream = s
            .execute()
            .unwrap()
            .execute(treasury_core::exec::ExecRequest::new(format!(
                "CREATE USER 'cert_user'@'%' IDENTIFIED BY '{PASSWORD}' REQUIRE X509"
            )))
            .await
            .unwrap();
        use futures::StreamExt;
        while let Some(e) = stream.next().await {
            e.unwrap();
        }
        check(&MySqlDriver::mysql(), root_cases).await;
        let mut cert_base = base.clone();
        cert_base.user = Some("cert_user".into());
        let cert_cases: Vec<Case> = cases(&cert_base, dir.path(), &pki, true)
            .into_iter()
            .filter(|c| c.label.contains("custom CA") || c.label.contains("client certificate"))
            .collect();
        assert_eq!(cert_cases.len(), 2);
        check(&MySqlDriver::mysql(), cert_cases).await;
    }
}
