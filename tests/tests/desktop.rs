//! The desktop app end to end. `tauri-driver` starts the real app, and the test works its window
//! the way a person would, against PostgreSQL in a container: add a connection from a connection
//! string, run statements, read the grid, edit rows in place after reading the statements the
//! edits become, and confirm a DELETE after seeing its row count.
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
    use treasury_tests::webdriver::{Session, TauriDriver, key};

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
    async fn connect_query_edit_and_confirm_a_delete() {
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
        // The release feed the app checks at start, served here with a newer version.
        let feed = serve_feed();
        let driver = TauriDriver::start(
            &native_driver(),
            &[
                ("XDG_DATA_HOME", &data),
                ("XDG_CACHE_HOME", &cache),
                ("TREASURY_UPDATE_URL", Path::new(&feed)),
            ],
        );
        let s = driver.session(&app);
        let shots = std::env::var_os("TREASURY_SCREENSHOTS").map(PathBuf::from);
        let shot = |name: &str| {
            if let Some(dir) = &shots {
                s.screenshot(&dir.join(format!("{name}.png")));
            }
        };

        // First launch: the walkthrough is there, and the newer version is offered.
        s.wait_text(".home h1", "Connect to a database", 60);
        s.wait_text(".tour", "Your first five minutes", 10);
        s.wait_text(".statusbar .update", "Treasury 99.0.0 is available", 30);
        shot("1-home");
        s.click_css(".statusbar .update .btn:not(.primary)");
        assert!(s.find(".statusbar .update").is_none());
        s.click_css(".tour .hide-tour");
        assert!(s.find(".tour").is_none());

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

        // Editing in place: change a cell, mark a row for deletion, read the statements, save.
        s.wait_text(".result-foot .edit-state", "can be edited", 30);
        let two = s.wait(&cell(2, 2), 10);
        s.script_on(
            &two,
            "arguments[0].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))",
        );
        // The editor opens with its text selected, so typing replaces it. WebDriver's own clear
        // would blur the field, which saves the edit and closes the editor.
        let input = s.wait(".cell-input", 10);
        s.script_on(
            &input,
            "arguments[0].value = 'deux'; \
             arguments[0].dispatchEvent(new Event('input', { bubbles: true }))",
        );
        s.keys(&input, &key::ENTER.to_string());
        s.wait_text(".result-foot .pending", "1 change", 10);
        s.click_css(&cell(3, 1));
        s.click_css(".result-foot .btn.delete-row");
        s.wait_text(".result-foot .pending", "2 changes", 10);
        shot("4-edits");
        s.click_css(".result-foot .btn.primary");
        let review = s.wait_text(".dialog .statements", "UPDATE", 30);
        assert!(
            review.contains("'deux'") && review.contains("DELETE FROM"),
            "{review}"
        );
        shot("5-review");
        s.click_css(".dialog .btn.primary");
        s.wait_text(".statusbar", "Saved. 2 rows changed.", 30);
        // The statement runs again, so the grid shows the rows as they are now.
        s.wait_text(".grid table", "deux", 30);
        s.wait_text(".result-foot", "2 rows", 30);

        // A DELETE on a shared database shows its row count first.
        run(&s, "DELETE FROM smoke WHERE id = 1");
        let dialog = s.wait_text(".dialog", "It changes 1 row", 30);
        assert!(dialog.contains("smoke"), "{dialog}");
        shot("6-confirm");
        s.click_css(".dialog .btn.primary");
        s.wait_text(".grid .message", "1 row changed", 30);

        run(&s, "SELECT id, name FROM smoke");
        s.wait_text(".result-foot", "1 row", 30);
        assert_eq!(s.text(&s.wait(&cell(1, 2), 10)), "deux");
        shot("7-after");

        // Tips can be put away; Settings switches the theme and brings the walkthrough back.
        s.click_css(".hint .no-tips");
        assert!(s.find(".hint").is_none());
        s.click_css(".brand .settings");
        let theme = s.wait("#st-theme", 10);
        s.script_on(
            &theme,
            "arguments[0].value = 'dark'; \
             arguments[0].dispatchEvent(new Event('change', { bubbles: true }))",
        );
        s.click_css(".dialog .btn.primary");
        s.wait_script(
            "return document.documentElement.dataset.theme === 'dark'",
            10,
        );
        shot("8-dark");
        s.click_css(".brand .settings");
        s.wait("#st-theme", 10);
        shot("9-settings");
        s.click_css(".dialog .walkthrough");
        s.wait_text(".tour", "Your first five minutes", 10);

        // The connection outlives the window.
        drop(s);
        drop(driver);
        assert!(saved_connection(&data), "the connection was not saved");
    }

    /// Serves an update feed announcing version 99.0.0 on a local port, and returns its URL.
    fn serve_feed() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!(
            "http://127.0.0.1:{}/latest.json",
            listener.local_addr().unwrap().port()
        );
        let platform = |p: &str| {
            format!(
                "\"{p}\": {{ \"signature\": \"unused\", \
                 \"url\": \"http://127.0.0.1:9/treasury.AppImage\" }}"
            )
        };
        let body = format!(
            "{{ \"version\": \"99.0.0\", \"notes\": \"A test release.\", \
             \"pub_date\": \"2026-11-26T00:00:00Z\", \"platforms\": {{ {}, {} }} }}",
            platform("linux-x86_64"),
            platform("linux-aarch64")
        );
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    /// The grid cell at `row` (from 1) and `column` (from 1, after the row number). The first
    /// body row is the spacer above the rows in view.
    fn cell(row: usize, column: usize) -> String {
        format!(
            ".grid tbody tr:nth-child({}) td:nth-child({})",
            row + 1,
            column + 1
        )
    }

    /// Whether the app's database under `data` holds a connection.
    fn saved_connection(data: &Path) -> bool {
        let dir = data.join("io.github.sepehrmohseni.treasury");
        std::fs::metadata(dir.join("treasury.db")).is_ok_and(|m| m.len() > 0)
    }
}
