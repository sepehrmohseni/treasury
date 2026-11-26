//! The command line against a real PostgreSQL server: queries, exports and the guardrails.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod docker {
    use std::process::{Command, Output, Stdio};
    use std::time::Duration;
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{GenericImage, ImageExt};
    use treasury_tests::docker::with_startup;

    const PASSWORD: &str = "cli-password";

    fn treasury(url: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_treasury"));
        cmd.args(&args[..1]).arg(url).args(&args[1..]);
        cmd.env("TREASURY_PASSWORD", PASSWORD)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn text(o: &Output) -> String {
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn errors(o: &Output) -> String {
        String::from_utf8_lossy(&o.stderr).into_owned()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queries_and_exports_go_through_the_guardrails() {
        let pg = with_startup(
            GenericImage::new("postgres", "19.0-alpine")
                .with_exposed_port(5432.tcp())
                .with_wait_for(WaitFor::message_on_stdout(
                    "PostgreSQL init process complete",
                )),
        )
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .start()
        .await
        .unwrap();
        let host = pg.get_host().await.unwrap().to_string();
        let port = pg.get_host_port_ipv4(5432.tcp()).await.unwrap();
        let url = format!("postgres://postgres@{host}:{port}/postgres?sslmode=disable");

        let mut ready = false;
        for _ in 0..120 {
            let o = treasury(&url, &["ping"]);
            if o.status.success() {
                assert!(text(&o).starts_with("PostgreSQL 19"), "{}", text(&o));
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(ready, "postgres never answered");

        let ok = |args: &[&str]| {
            let o = treasury(&url, args);
            assert!(o.status.success(), "{args:?}: {}", errors(&o));
            text(&o)
        };
        assert_eq!(
            ok(&["query", "--write", "CREATE TABLE t (id int, name text)"]),
            "Done.\n"
        );

        // Read-only unless asked.
        let refused = treasury(&url, &["query", "INSERT INTO t VALUES (1, 'ada')"]);
        assert!(!refused.status.success());
        assert!(errors(&refused).contains("--write"), "{}", errors(&refused));

        assert_eq!(
            ok(&[
                "query",
                "--write",
                "INSERT INTO t VALUES (1, 'ada'), (2, 'bob')"
            ]),
            "2 rows changed.\n"
        );

        // An UPDATE without WHERE needs the connection name typed at a terminal.
        let unfiltered = treasury(&url, &["query", "--write", "UPDATE t SET name = 'x'"]);
        assert!(!unfiltered.status.success());
        assert!(
            errors(&unfiltered).contains("This changes 2 rows."),
            "{}",
            errors(&unfiltered)
        );
        assert!(
            errors(&unfiltered).contains("terminal"),
            "{}",
            errors(&unfiltered)
        );

        assert_eq!(
            ok(&["query", "SELECT id, name FROM t ORDER BY id"]),
            "id  name\n--  ----\n 1  ada\n 2  bob\n(2 rows)\n"
        );
        assert_eq!(
            ok(&[
                "query",
                "--format",
                "csv",
                "SELECT id, name FROM t ORDER BY id"
            ]),
            "id,name\n1,ada\n2,bob\n"
        );

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.csv");
        let report = ok(&[
            "export",
            "--format",
            "csv",
            "--out",
            out.to_str().unwrap(),
            "SELECT id, name FROM t ORDER BY id",
        ]);
        assert!(report.starts_with("Wrote 2 rows"), "{report}");
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "id,name\n1,ada\n2,bob\n"
        );
    }
}
