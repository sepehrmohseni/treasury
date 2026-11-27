//! The desktop app end to end. `tauri-driver` starts the real app, and the test works its window
//! the way a person would, against PostgreSQL in a container: add a connection from a connection
//! string, run statements, read the grid, edit rows in place after reading the statements the
//! edits become, confirm a DELETE after seeing its row count, work in several tabs and save one
//! as a query, search the history, and find the tabs and the history again after a restart.
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
    use treasury_tests::docker::{sshd_image, with_startup};
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
        // An SSH server that reaches the database on the containers' network.
        let sshd = with_startup(sshd_image().await)
            .start()
            .await
            .expect("start sshd");
        let ssh_host = sshd.get_host().await.unwrap().to_string();
        let ssh_port = sshd.get_host_port_ipv4(22.tcp()).await.unwrap();
        let pg_inside = pg.get_bridge_ip_address().await.unwrap();

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
        // The page's own script ran (the content security policy allows it) and saw no errors.
        assert_eq!(
            s.script("return window.__startupErrors"),
            serde_json::json!([])
        );
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
        s.wait_text(&shown(".grid .message"), "Done", 30);
        run(
            &s,
            "INSERT INTO smoke VALUES (1, 'one'), (2, 'two'), (3, NULL)",
        );
        s.wait_text(&shown(".grid .message"), "3 rows changed", 30);

        run(&s, "SELECT id, name FROM smoke ORDER BY id");
        let grid = s.wait_text(&shown(".grid table"), "two", 30);
        assert!(grid.contains("one") && grid.contains("NULL"), "{grid}");
        s.wait_text(&shown(".result-foot"), "3 rows", 10);
        shot("3-grid");

        // Editing in place: change a cell, mark a row for deletion, read the statements, save.
        s.wait_text(&shown(".result-foot .edit-state"), "can be edited", 30);
        let two = s.wait(&cell(2, 2), 10);
        s.double_click(&two);
        // The editor opens with its text selected, so typing replaces it. WebDriver's own clear
        // would blur the field, which saves the edit and closes the editor.
        let input = s.wait(".cell-input", 10);
        s.script_on(
            &input,
            "arguments[0].value = 'deux'; \
             arguments[0].dispatchEvent(new Event('input', { bubbles: true }))",
        );
        s.keys(&input, &key::ENTER.to_string());
        s.wait_text(&shown(".result-foot .pending"), "1 change", 10);
        s.click_css(&cell(3, 1));
        s.click_css(&shown(".result-foot .btn.delete-row"));
        s.wait_text(&shown(".result-foot .pending"), "2 changes", 10);
        shot("4-edits");
        s.click_css(&shown(".result-foot .btn.primary"));
        let review = s.wait_text(".dialog .statements", "UPDATE", 30);
        assert!(
            review.contains("'deux'") && review.contains("DELETE FROM"),
            "{review}"
        );
        shot("5-review");
        s.click_css(".dialog .btn.primary");
        s.wait_text(".statusbar", "Saved. 2 rows changed.", 30);
        // The statement runs again, so the grid shows the rows as they are now.
        s.wait_text(&shown(".grid table"), "deux", 30);
        s.wait_text(&shown(".result-foot"), "2 rows", 30);

        // A DELETE on a shared database shows its row count first.
        run(&s, "DELETE FROM smoke WHERE id = 1");
        let dialog = s.wait_text(".dialog", "It changes 1 row", 30);
        assert!(dialog.contains("smoke"), "{dialog}");
        shot("6-confirm");
        s.click_css(".dialog .btn.primary");
        s.wait_text(&shown(".grid .message"), "1 row changed", 30);

        run(&s, "SELECT id, name FROM smoke");
        s.wait_text(&shown(".result-foot"), "1 row", 30);
        assert_eq!(s.text(&s.wait(&cell(1, 2), 10)), "deux");
        shot("7-after");

        // The tree: a table double-clicked shows its rows, in a tab of its own.
        s.click(&s.wait_item(".tree .item", "public", 10));
        s.click(&s.wait_item(".tree .item", "Tables", 10));
        let table = s.wait_item(".tree .item", "smoke", 10);
        s.double_click(&table);
        s.wait_text(".tab-strip .etab.on", "smoke", 10);
        s.wait_text(".editor pre", "SELECT * FROM", 10);
        s.wait_text(&shown(".grid table"), "deux", 30);
        assert_eq!(tab_count(&s), 2);
        shot("8-tree");

        // Tabs: each keeps its text and its result while another is shown.
        s.click_css(".tab-strip .etab-new");
        s.wait_text(".tab-strip .etab.on", "Query 2", 10);
        run(&s, "SELECT 'second tab' AS note");
        s.wait_text(&shown(".grid table"), "second tab", 30);
        s.click(&s.wait_item(".tab-strip .etab-title", "Query 1", 10));
        s.wait_text(".editor pre", "SELECT id, name FROM smoke", 10);
        assert_eq!(s.text(&s.wait(&cell(1, 2), 10)), "deux");

        // A tab saved as a query, in a folder, takes the query's name.
        s.click(&s.wait_item(".tab-strip .etab-title", "Query 2", 10));
        s.wait_text(".editor pre", "second tab", 10);
        s.click_css(".toolbar .save-query");
        let name = s.wait("#sq-name", 10);
        s.keys(&name, "Second tab");
        s.keys(&s.wait("#sq-folder", 10), "checks / daily");
        shot("9-save-query");
        s.click_css(".dialog button[type=submit]");
        s.wait_text(".statusbar", "Saved \"Second tab\".", 10);
        s.wait_text(".tab-strip .etab.on", "Second tab", 10);
        s.click_css(".side-views .view-queries");
        let list = s.wait_text(".list.queries", "checks/daily", 10);
        assert!(list.contains("Second tab"), "{list}");
        shot("10-tabs-and-queries");

        // Changing the text marks the tab until Ctrl+S saves the query again.
        let editor = s.wait(".editor textarea", 10);
        s.keys(&editor, " ");
        s.wait(".tab-strip .etab.on .changed-dot", 10);
        s.keys(&editor, &format!("{}s{}", key::CONTROL, key::NULL));
        s.wait_script(
            "return !document.querySelector('.tab-strip .etab.on .changed-dot')",
            10,
        );

        // A tab whose text is saved closes without asking, and the query opens it again.
        s.click_css(".tab-strip .etab.on .etab-close");
        s.wait_script(
            "return document.querySelectorAll('.tab-strip .etab').length === 2",
            10,
        );
        s.click(&s.wait_item(".list.queries .item", "Second tab", 10));
        s.wait_text(".tab-strip .etab.on", "Second tab", 10);
        s.wait_text(".editor pre", "second tab", 10);

        // A tab with text saved nowhere else asks first.
        s.click_css(".tab-strip .etab-new");
        s.wait_text(".tab-strip .etab.on", "Query 2", 10);
        s.keys(&s.wait(".editor textarea", 10), "SELECT 'draft'");
        s.click_css(".tab-strip .etab.on .etab-close");
        s.wait_text(".dialog", "not saved as a query", 10);
        s.click_css(".dialog footer .btn:first-child");
        assert!(s.find(".dialog").is_none());
        assert_eq!(tab_count(&s), 4);

        // History: what ran on this connection, searchable; an entry opens in a new tab, since
        // the one shown holds text.
        s.click_css(".side-views .view-history");
        s.wait_text(".list.history", "SELECT 'second tab' AS note", 10);
        s.keys(&s.wait("#history-search", 10), "insert");
        s.wait_script(
            "const e = [...document.querySelectorAll('.list.history .entry-text')]; \
             return e.length === 1 && e[0].innerText.startsWith('INSERT INTO smoke')",
            10,
        );
        shot("11-history");
        s.click_css(".list.history .entry");
        s.wait_text(".tab-strip .etab.on", "Query 3", 10);
        s.wait_text(".editor pre", "INSERT INTO smoke", 10);

        // Format lays the statement out on lines, as one edit that can be undone.
        s.click_css(".toolbar .format");
        s.wait_text(".editor pre", "VALUES\n", 10);
        // WebDriver's keys do not reach WebKit's editing commands, so the page is asked to undo,
        // as Ctrl+Z does for a person.
        assert_eq!(
            s.script("return document.execCommand('undo')"),
            serde_json::json!(true)
        );
        s.wait_text(".editor pre", "INSERT INTO smoke VALUES (1, 'one')", 10);

        // Run all: every statement in turn, each with its own result.
        let editor = s.wait(".editor textarea", 10);
        s.clear(&editor);
        s.keys(
            &editor,
            "CREATE TABLE script_t (n int);\nINSERT INTO script_t VALUES (1), (2);\n\
             SELECT n FROM script_t ORDER BY n",
        );
        s.click_css(".toolbar .run-all");
        s.wait_text(".statusbar", "Ran all 3 statements.", 30);
        let tabs = s.wait_text(&shown(".result-tabs"), "3 · Result", 30);
        assert!(
            tabs.contains("1 · Done") && tabs.contains("2 · 2 rows changed"),
            "{tabs}"
        );
        assert_eq!(s.text(&s.wait(&cell(2, 1), 10)), "2");
        shot("12-run-all");
        s.click(&s.wait_item(&shown(".result-tabs button"), "2 · 2 rows", 10));
        s.wait_text(&shown(".grid .message"), "2 rows changed", 10);

        // A script stops at the first statement that fails.
        s.clear(&editor);
        s.keys(
            &editor,
            "SELECT 1 AS one; SELECT * FROM no_such_table; SELECT 3 AS three",
        );
        s.click_css(".toolbar .run-all");
        let bar = s.wait_text(
            ".statusbar",
            "Statement 2 of 3 failed, so the ones after it did not run",
            30,
        );
        assert!(bar.contains("no_such_table"), "{bar}");
        let tabs = s.text(&s.wait(&shown(".result-tabs"), 10));
        assert!(!tabs.contains("3") && !tabs.contains("2 ·"), "{tabs}");

        // Completion: a table after FROM, then a column of the table the statement names.
        s.clear(&editor);
        s.keys(&editor, "SELECT * FROM smo");
        s.wait_text(".completions", "smoke", 30);
        shot("13-complete");
        s.keys(&editor, &key::ENTER.to_string());
        s.wait_text(".editor pre", "SELECT * FROM smoke", 10);
        s.keys(&editor, " WHERE na");
        s.wait_text(".completions", "name", 30);
        s.keys(&editor, &key::ENTER.to_string());
        s.wait_text(".editor pre", "SELECT * FROM smoke WHERE name", 10);
        assert!(s.find(".completions").is_none());

        // The command palette: Ctrl+K, a few letters of a table's name, Enter shows its rows.
        s.keys(&editor, &format!("{}k{}", key::CONTROL, key::NULL));
        let input = s.wait("#palette-input", 10);
        s.keys(&input, "scrt");
        s.wait_text(".palette-list li.on", "script_t", 30);
        shot("14-palette");
        s.keys(&input, &key::ENTER.to_string());
        assert!(s.find(".palette").is_none());
        s.wait_text(".tab-strip .etab.on", "script_t", 10);
        s.wait_text(&shown(".result-foot"), "2 rows", 30);
        s.click_css(".side-views .view-connections");

        // A production connection opens read-only, refuses writes until they are unlocked, and
        // then wants its name typed for a DELETE without WHERE.
        s.click_css(".side-actions .btn.primary");
        let uri = s.wait("#cf-uri", 10);
        s.keys(
            &uri,
            &format!("postgres://postgres:{PASSWORD}@{host}:{port}/treasury?sslmode=disable"),
        );
        let name = s.wait("#cf-name", 10);
        s.click(&name);
        s.keys(&name, "smoke-prod");
        let env = s.wait("#cf-env", 10);
        s.script_on(
            &env,
            "arguments[0].value = 'Production'; \
             arguments[0].dispatchEvent(new Event('change', { bubbles: true }))",
        );
        s.wait_text(".dialog", "Production connections open read-only", 10);
        s.click_css(".dialog button[type=submit]");
        s.wait_text(".toolbar .title", "smoke-prod", 30);
        s.wait(".toolbar .badge.ro", 30);
        shot("15-production");
        run(&s, "DELETE FROM smoke");
        s.wait_text(".dialog", "Unlock writes to run it", 30);
        shot("16-refused");
        s.click_css(".dialog .btn.primary");
        s.wait_text(".statusbar", "Writes unlocked for 15 minutes", 30);
        run(&s, "DELETE FROM smoke");
        s.wait_text(".dialog", "It changes 1 row", 30);
        let typed = s.wait("#gd-name", 10);
        s.keys(&typed, "smoke-prod");
        s.wait_script(
            "return document.querySelector('.dialog .btn.primary').disabled === false",
            10,
        );
        shot("17-type-the-name");
        s.click_css(".dialog .btn.primary");
        s.wait_text(&shown(".grid .message"), "1 row changed", 30);

        // Production connections keep no history unless their settings say so.
        s.click_css(".side-views .view-history");
        let history = s.wait_text(".list.history", "keeps no history", 10);
        assert!(history.contains("No statements have run"), "{history}");
        s.click_css(".side-views .view-connections");

        // An SSH tunnel set up in the form, to the database as the SSH host reaches it; the
        // host key is trusted when asked.
        s.click_css(".side-actions .btn.primary");
        let uri = s.wait("#cf-uri", 10);
        s.keys(
            &uri,
            &format!("postgres://postgres:{PASSWORD}@{pg_inside}:5432/treasury?sslmode=disable"),
        );
        let name = s.wait("#cf-name", 10);
        s.click(&name);
        s.keys(&name, "via-ssh");
        s.click_css("details.ssh > summary");
        s.click_css("#cf-ssh-on");
        s.keys(&s.wait("#cf-ssh-host", 10), &ssh_host);
        let p = s.wait("#cf-ssh-port", 10);
        s.script_on(
            &p,
            &format!(
                "arguments[0].value = '{ssh_port}'; \
                 arguments[0].dispatchEvent(new Event('input', {{ bubbles: true }}))"
            ),
        );
        s.keys(&s.wait("#cf-ssh-user", 10), "treasury");
        s.script_on(
            &s.wait("#cf-ssh-auth", 10),
            "arguments[0].value = 'Password'; \
             arguments[0].dispatchEvent(new Event('change', { bubbles: true }))",
        );
        s.keys(&s.wait("#cf-ssh-secret", 10), "tunnel-password");
        shot("18-ssh");
        s.click_css(".dialog button[type=submit]");
        s.wait_text(".dialog", "Unknown SSH host", 30);
        s.click_css(".dialog .btn.primary");
        s.wait_text(".toolbar .title", "via-ssh", 30);
        s.wait_text(".statusbar", "PostgreSQL", 60);
        run(&s, "SELECT inet_server_addr() AS server");
        s.wait_text(&shown(".result-foot"), "1 row", 30);

        // Deleting a connection asks first.
        s.double_click(&s.wait_item(".list > .item", "via-ssh", 10));
        s.click_css(".dialog .delete-connection");
        s.wait_text(".dialog", "cannot be undone", 10);
        s.click_css(".dialog .confirm-delete");
        s.wait_script(
            "return ![...document.querySelectorAll('.list > .item')].some(e => e.innerText.includes('via-ssh'))",
            10,
        );
        s.click(&s.wait_item(".list > .item", "smoke-prod", 10));
        s.wait_text(".toolbar .title", "smoke-prod", 30);

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
        // Controls take the theme's colours too, not the system's white.
        s.wait_script(
            "return getComputedStyle(document.querySelector('.brand .settings')).backgroundColor \
             !== 'rgb(255, 255, 255)'",
            10,
        );
        s.wait_text(&shown(".result-tabs"), "1 row changed", 10);
        shot("19-dark");
        s.click_css(".brand .settings");
        s.wait("#st-theme", 10);
        shot("20-settings");
        s.click_css(".dialog .walkthrough");
        s.wait_text(".tour", "Your first five minutes", 10);

        // After a restart: the password came back from the keychain, and the tabs with their
        // text, the saved query and the draft among them.
        drop(s);
        assert!(saved_connection(&data), "the connection was not saved");
        let s = driver.session(&app);
        s.wait_text(".home h1", "Connect to a database", 60);
        assert!(
            !s.page_text().contains("kept only until Treasury quits"),
            "the app found no keychain"
        );
        s.click(&s.wait_item(".list > .item", "smoke", 30));
        s.wait_text(".statusbar", "PostgreSQL", 60);
        let strip = s.wait_text(".tab-strip", "Second tab", 30);
        for title in ["Query 1", "smoke", "Query 2", "Query 3", "script_t"] {
            assert!(strip.contains(title), "{strip}");
        }
        s.click(&s.wait_item(".tab-strip .etab-title", "Query 2", 10));
        s.wait_text(".editor pre", "SELECT 'draft'", 10);
        run(&s, "SELECT count(*) AS n FROM smoke");
        s.wait_text(&shown(".result-foot"), "1 row", 30);

        // The history kept from before the restart, and clearing it.
        s.click_css(".side-views .view-history");
        s.wait_text(".list.history", "SELECT 'second tab' AS note", 10);
        s.click_css(".list.history .clear-history");
        s.wait_text(".dialog", "cannot be undone", 10);
        s.click_css(".dialog .btn.danger");
        s.wait_text(".statusbar", "Cleared the history of \"smoke\"", 10);
        s.wait_text(".list.history", "No statements have run", 10);
        drop(s);
        drop(driver);
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

    /// `css` inside the result pane of the tab shown; the other tabs keep theirs, hidden.
    fn shown(css: &str) -> String {
        format!(".pane:not([hidden]) {css}")
    }

    /// The grid cell at `row` (from 1) and `column` (from 1, after the row number) of the tab
    /// shown. The first body row is the spacer above the rows in view.
    fn cell(row: usize, column: usize) -> String {
        shown(&format!(
            ".grid tbody tr:nth-child({}) td:nth-child({})",
            row + 1,
            column + 1
        ))
    }

    /// How many tabs the strip shows.
    fn tab_count(s: &Session) -> u64 {
        s.script("return document.querySelectorAll('.tab-strip .etab').length")
            .as_u64()
            .unwrap()
    }

    /// Whether the app's database under `data` holds a connection.
    fn saved_connection(data: &Path) -> bool {
        let dir = data.join("io.github.sepehrmohseni.treasury");
        std::fs::metadata(dir.join("treasury.db")).is_ok_and(|m| m.len() > 0)
    }
}
