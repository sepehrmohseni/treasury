//! The desktop app end to end. `tauri-driver` starts the real app, and the test works its window
//! the way a person would, against PostgreSQL in a container: add a connection from a connection
//! string, run statements, read the grid, and confirm a DELETE after seeing its row count.
//!
//! It runs through `cargo xtask ci desktop`, which builds the app, starts a display when there is
//! none and sets `TREASURY_APP` to the binary.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod desktop {
    use std::path::{Path, PathBuf};
    use testcontainers::core::wait::LogWaitStrategy;
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{GenericImage, ImageExt};
    use treasury_tests::docker::with_startup;
    use treasury_tests::webdriver::{Session, TauriDriver};

    const PASSWORD: &str = "desktop-password";

    fn native_driver() -> PathBuf {
        std::env::var_os("TREASURY_NATIVE_DRIVER")
            .map_or_else(|| PathBuf::from("/usr/bin/WebKitWebDriver"), PathBuf::from)
    }

    /// Replaces the editor's text with `sql` and runs it with the toolbar button.
    fn run(s: &Session, sql: &str) {
        let editor = s.wait(".editor textarea", 10);
        s.clear(&editor);
        s.keys(&editor, sql);
        s.click_css(".toolbar .btn.primary");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_query_and_confirm_a_delete() {
        let app = PathBuf::from(
            std::env::var_os("TREASURY_APP")
                .expect("TREASURY_APP is not set; run this through `cargo xtask ci desktop`"),
        );
        let pg = with_startup(
            GenericImage::new("postgres", "19.0-alpine")
                .with_exposed_port(5432.tcp())
                // The image starts the server twice: once to initialise, then for real.
                .with_wait_for(WaitFor::log(
                    LogWaitStrategy::stdout_or_stderr(
                        "database system is ready to accept connections",
                    )
                    .with_times(2),
                )),
        )
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_DB", "treasury")
        .start()
        .await
        .expect("start postgres");
        let host = pg.get_host().await.unwrap().to_string();
        let port = pg.get_host_port_ipv4(5432.tcp()).await.unwrap();

        // A fresh profile, so the run starts from the first-launch screen.
        let profile = tempfile::tempdir().unwrap();
        let data = profile.path().join("data");
        let cache = profile.path().join("cache");
        let driver = TauriDriver::start(
            &native_driver(),
            &[("XDG_DATA_HOME", &data), ("XDG_CACHE_HOME", &cache)],
        );
        let s = driver.session(&app);
        let shots = std::env::var_os("TREASURY_SCREENSHOTS").map(PathBuf::from);
        let shot = |name: &str| {
            if let Some(dir) = &shots {
                s.screenshot(&dir.join(format!("{name}.png")));
            }
        };

        // First launch: the walkthrough is there and can be put away.
        s.wait_text(".home h1", "Connect to a database", 60);
        s.wait_text(".tour", "Your first five minutes", 10);
        shot("1-home");

        // A connection from a connection string.
        s.click_css(".home .actions .btn.primary");
        let uri = s.wait("#cf-uri", 10);
        s.keys(
            &uri,
            &format!("postgres://postgres:{PASSWORD}@{host}:{port}/treasury?sslmode=disable"),
        );
        let name = s.wait("#cf-name", 10);
        s.click(&name);
        s.keys(&name, "smoke");
        shot("2-form");
        s.click_css(".dialog button[type=submit]");

        // Connected: the toolbar names it and the server answers.
        s.wait_text(".toolbar .title", "smoke", 30);
        s.wait_text(".statusbar", "PostgreSQL", 60);

        run(&s, "CREATE TABLE smoke (id int PRIMARY KEY, name text)");
        s.wait_text(".grid .message", "Done", 30);
        run(
            &s,
            "INSERT INTO smoke VALUES (1, 'one'), (2, 'two'), (3, NULL)",
        );
        s.wait_text(".grid .message", "3 rows changed", 30);

        run(&s, "SELECT id, name FROM smoke ORDER BY id");
        let grid = s.wait_text(".grid table", "two", 30);
        assert!(grid.contains("one") && grid.contains("NULL"), "{grid}");
        s.wait_text(".result-foot", "3 rows", 10);
        shot("3-grid");

        // A DELETE on a shared database shows its row count first.
        run(&s, "DELETE FROM smoke WHERE id = 1");
        let dialog = s.wait_text(".dialog", "It changes 1 row", 30);
        assert!(dialog.contains("smoke"), "{dialog}");
        shot("4-confirm");
        s.click_css(".dialog .btn.primary");
        s.wait_text(".grid .message", "1 row changed", 30);

        run(&s, "SELECT count(*) AS left_over FROM smoke");
        s.wait_text(".grid table", "2", 30);
        shot("5-after");

        // The connection outlives the window.
        drop(s);
        drop(driver);
        assert!(saved_connection(&data), "the connection was not saved");
    }

    /// Whether the app's database under `data` holds a connection.
    fn saved_connection(data: &Path) -> bool {
        let dir = data.join("io.github.sepehrmohseni.treasury");
        std::fs::metadata(dir.join("treasury.db")).is_ok_and(|m| m.len() > 0)
    }
}
