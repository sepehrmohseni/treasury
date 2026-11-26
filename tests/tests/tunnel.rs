//! SSH tunnels against real OpenSSH servers in containers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod docker {
    use async_trait::async_trait;
    use russh::keys::ssh_key::LineEnding;
    use russh::keys::{Algorithm, PrivateKey};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use testcontainers::core::{ContainerPort, IntoContainerPort};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, GenericImage, ImageExt};
    use tokio::io::AsyncReadExt;
    use treasury_core::ErrorKind;
    use treasury_core::config::{SecretSlot, SshAuth, SshConfig, SshHop};
    use treasury_tests::MapSecrets;
    use treasury_tests::docker::{sshd_image, with_startup};
    use treasury_tunnel::{HostKeyPrompt, KnownHosts, Tunnel, TunnelParams};

    struct Prompt {
        accept: bool,
        asked: AtomicUsize,
    }

    #[async_trait]
    impl HostKeyPrompt for Prompt {
        async fn confirm_new(&self, _: &str, _: u16, _: &str, fingerprint: &str) -> bool {
            assert!(fingerprint.starts_with("SHA256:"));
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.accept
        }
    }

    fn prompt(accept: bool) -> Arc<Prompt> {
        Arc::new(Prompt {
            accept,
            asked: AtomicUsize::new(0),
        })
    }

    async fn sshd(env: &[(&str, &str)]) -> (ContainerAsync<GenericImage>, String, u16) {
        let mut req = with_startup(sshd_image().await);
        for (k, v) in env {
            req = req.with_env_var(*k, *v);
        }
        let c = req.start().await.expect("start sshd");
        let host = c.get_host().await.unwrap().to_string();
        let port = c.get_host_port_ipv4(22.tcp()).await.unwrap();
        (c, host, port)
    }

    fn hop(host: &str, port: u16, auth: SshAuth) -> SshHop {
        SshHop {
            host: host.into(),
            port,
            user: "treasury".into(),
            auth,
        }
    }

    fn password_secrets() -> MapSecrets {
        let mut s = MapSecrets::default();
        s.0.insert(SecretSlot::SshPassword, "tunnel-password".into());
        s.0.insert(SecretSlot::JumpPassword, "tunnel-password".into());
        s
    }

    /// Reads the SSH banner through the tunnel: the target is the server's own sshd.
    async fn banner(t: &Tunnel) -> String {
        let mut s = tokio::net::TcpStream::connect(t.local_addr())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    async fn open(
        ssh: &SshConfig,
        target: (&str, u16),
        secrets: &MapSecrets,
        kh: &KnownHosts,
        p: Arc<Prompt>,
    ) -> treasury_core::Result<Tunnel> {
        Tunnel::open(TunnelParams {
            ssh,
            target,
            secrets,
            known_hosts: kh,
            prompt: p,
            timeout: Duration::from_secs(20),
        })
        .await
    }

    fn known_hosts(dir: &tempfile::TempDir) -> KnownHosts {
        KnownHosts {
            app_file: dir.path().join("known_hosts"),
            user_file: None,
        }
    }

    #[tokio::test]
    async fn password_tunnel_forwards_and_remembers_the_host_key() {
        let (_c, host, port) = sshd(&[]).await;
        let dir = tempfile::tempdir().unwrap();
        let kh = known_hosts(&dir);
        let ssh = SshConfig {
            target: hop(&host, port, SshAuth::Password),
            jump: None,
        };
        let p = prompt(true);
        let t = open(&ssh, ("127.0.0.1", 22), &password_secrets(), &kh, p.clone())
            .await
            .unwrap();
        assert!(banner(&t).await.starts_with("SSH-2.0-OpenSSH"));
        assert_eq!(p.asked.load(Ordering::SeqCst), 1);
        drop(t);
        // The key is now known: no second prompt.
        let again = open(&ssh, ("127.0.0.1", 22), &password_secrets(), &kh, p.clone())
            .await
            .unwrap();
        assert!(banner(&again).await.starts_with("SSH-2.0"));
        assert_eq!(p.asked.load(Ordering::SeqCst), 1);
        let addr = again.local_addr();
        drop(again);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "the port closes with the tunnel"
        );
    }

    #[tokio::test]
    async fn key_file_login() {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let (_c, host, port) = sshd(&[("AUTHORIZED_KEY", &public)]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_ed25519");
        std::fs::write(&path, key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        let ssh = SshConfig {
            target: hop(&host, port, SshAuth::KeyFile { path }),
            jump: None,
        };
        let t = open(
            &ssh,
            ("127.0.0.1", 22),
            &MapSecrets::default(),
            &known_hosts(&dir),
            prompt(true),
        )
        .await
        .unwrap();
        assert!(banner(&t).await.starts_with("SSH-2.0"));
    }

    #[tokio::test]
    async fn wrong_password_is_an_auth_error() {
        let (_c, host, port) = sshd(&[]).await;
        let dir = tempfile::tempdir().unwrap();
        let mut secrets = MapSecrets::default();
        secrets.0.insert(SecretSlot::SshPassword, "nope".into());
        let ssh = SshConfig {
            target: hop(&host, port, SshAuth::Password),
            jump: None,
        };
        let err = open(
            &ssh,
            ("127.0.0.1", 22),
            &secrets,
            &known_hosts(&dir),
            prompt(true),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.kind, ErrorKind::Auth);
    }

    #[tokio::test]
    async fn declined_and_changed_host_keys_stop_the_connection() {
        let (_c, host, port) = sshd(&[]).await;
        let dir = tempfile::tempdir().unwrap();
        let kh = known_hosts(&dir);
        let ssh = SshConfig {
            target: hop(&host, port, SshAuth::Password),
            jump: None,
        };
        let err = open(
            &ssh,
            ("127.0.0.1", 22),
            &password_secrets(),
            &kh,
            prompt(false),
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(err.kind, ErrorKind::SshHostKey { changed: false, .. }),
            "{err:?}"
        );
        // Record a different key for this host: the real one is now a change.
        let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        kh.learn(&host, port, other.public_key()).unwrap();
        let err = open(
            &ssh,
            ("127.0.0.1", 22),
            &password_secrets(),
            &kh,
            prompt(true),
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(err.kind, ErrorKind::SshHostKey { changed: true, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn through_a_jump_host() {
        let network = format!("treasury-jump-{}", std::process::id());
        let target_name = format!("treasury-target-{}", std::process::id());
        let target = with_startup(sshd_image().await)
            .with_network(&network)
            .with_container_name(&target_name)
            .start()
            .await
            .unwrap();
        let jump = with_startup(sshd_image().await)
            .with_network(&network)
            .start()
            .await
            .unwrap();
        let jump_host = jump.get_host().await.unwrap().to_string();
        let jump_port = jump
            .get_host_port_ipv4(ContainerPort::Tcp(22))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ssh = SshConfig {
            target: hop(&target_name, 22, SshAuth::Password),
            jump: Some(hop(&jump_host, jump_port, SshAuth::Password)),
        };
        let p = prompt(true);
        let t = open(
            &ssh,
            ("127.0.0.1", 22),
            &password_secrets(),
            &known_hosts(&dir),
            p.clone(),
        )
        .await
        .unwrap();
        assert!(banner(&t).await.starts_with("SSH-2.0"));
        assert_eq!(
            p.asked.load(Ordering::SeqCst),
            2,
            "both hops' keys were confirmed"
        );
        drop(t);
        drop(target);
    }
}
