//! A small W3C WebDriver client for driving the desktop app through `tauri-driver`.
//!
//! Only what the desktop tests use: find, click, type, read text, run a script, take a screenshot.
//! Calls are blocking and go to a driver on this machine over plain HTTP.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The key W3C WebDriver uses for element references.
const ELEMENT: &str = "element-6066-11e4-a52e-4f735466cecf";

/// Keys for [`Session::keys`], from the WebDriver key table.
pub mod key {
    /// Releases held modifiers.
    pub const NULL: char = '\u{E000}';
    /// Enter.
    pub const ENTER: char = '\u{E007}';
    /// Control.
    pub const CONTROL: char = '\u{E009}';
}

/// Fetches and compiles the page's WebAssembly, and says what happened.
const WASM_CHECK: &str = r#"
const done = arguments[arguments.length - 1];
const errors = (window.__startupErrors || []).join(" | ") || "none";
const page = location.href + " \"" + document.title + "\", Tauri " + (typeof window.__TAURI__) +
  ", startup errors: " + errors + "; ";
const link = [...document.querySelectorAll("link")].map(l => l.href).find(h => h.endsWith(".wasm"));
if (!link) { done(page + "no .wasm link in " + document.head.innerHTML.slice(0, 400)); return; }
fetch(link)
  .then(r => r.arrayBuffer())
  .then(b => WebAssembly.compile(b).then(() => page + "compiles " + link + " (" + b.byteLength + " bytes)"))
  .then(done, e => done(page + "fails " + link + ": " + e));
"#;

/// A running `tauri-driver`, stopped on drop.
pub struct TauriDriver {
    child: Child,
    port: u16,
    /// What tauri-driver, WebKitWebDriver and the app write to standard error.
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl TauriDriver {
    /// Starts `tauri-driver` on a free port, with `native` (WebKitWebDriver) behind it. `env` is
    /// passed to the app it starts.
    pub fn start(native: &Path, env: &[(&str, &Path)]) -> Self {
        let port = free_port();
        let native_port = free_port();
        let dir = tempfile::tempdir().expect("a directory for the app's log");
        let log = dir.path().join("app.log");
        let mut cmd = Command::new("tauri-driver");
        cmd.arg("--port")
            .arg(port.to_string())
            .arg("--native-port")
            .arg(native_port.to_string())
            .arg("--native-driver")
            .arg(native)
            // Its children (WebKitWebDriver, the app) inherit these. A file, not the test's own
            // output, which they would outlive; failure messages quote its end.
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(&log).expect("create the app's log"),
            ));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("start tauri-driver");
        let driver = Self {
            child,
            port,
            log,
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "tauri-driver did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        driver
    }

    /// Opens the app at `application` in a new session.
    pub fn session(&self, application: &Path) -> Session {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        let base = format!("http://127.0.0.1:{}", self.port);
        let caps = json!({ "capabilities": { "alwaysMatch": {
            "tauri:options": { "application": application }
        }}});
        let reply = post(&agent, &format!("{base}/session"), &caps);
        let id = reply["value"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no session: {reply}"))
            .to_owned();
        Session {
            agent,
            url: format!("{base}/session/{id}"),
            log: self.log.clone(),
        }
    }
}

impl Drop for TauriDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free port")
}

fn post(agent: &ureq::Agent, url: &str, body: &Value) -> Value {
    let mut reply = agent
        .post(url)
        .send_json(body)
        .unwrap_or_else(|e| panic!("POST {url}: {e}"));
    reply
        .body_mut()
        .read_json::<Value>()
        .unwrap_or_else(|e| panic!("POST {url}: {e}"))
}

/// An element in the window.
#[derive(Clone, Debug)]
pub struct Element(String);

/// One app window under test. The app closes when the session ends.
pub struct Session {
    agent: ureq::Agent,
    url: String,
    log: PathBuf,
}

impl Session {
    fn post(&self, path: &str, body: &Value) -> Value {
        post(&self.agent, &format!("{}{path}", self.url), body)
    }

    fn get(&self, path: &str) -> Value {
        let url = format!("{}{path}", self.url);
        let mut reply = self
            .agent
            .get(&url)
            .call()
            .unwrap_or_else(|e| panic!("GET {url}: {e}"));
        reply
            .body_mut()
            .read_json::<Value>()
            .unwrap_or_else(|e| panic!("GET {url}: {e}"))
    }

    /// The first element matching `css`, if there is one now.
    pub fn find(&self, css: &str) -> Option<Element> {
        let reply = self.post(
            "/element",
            &json!({ "using": "css selector", "value": css }),
        );
        if let Some(id) = reply["value"][ELEMENT].as_str() {
            return Some(Element(id.to_owned()));
        }
        // Not there yet is an answer; any other error is a broken session.
        let error = reply["value"]["error"].as_str().unwrap_or_default();
        assert_eq!(error, "no such element", "finding `{css}` failed: {reply}");
        None
    }

    /// Waits up to `secs` for an element matching `css`.
    pub fn wait(&self, css: &str, secs: u64) -> Element {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(e) = self.find(css) {
                return e;
            }
            assert!(
                Instant::now() < deadline,
                "nothing matched `{css}` within {secs}s; the window shows:\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Waits up to `secs` until the text of `css`'s first match contains `wanted`.
    pub fn wait_text(&self, css: &str, wanted: &str, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut last = None;
        loop {
            if let Some(e) = self.find(css) {
                let text = self.text(&e);
                if text.contains(wanted) {
                    return text;
                }
                last = Some(text);
            }
            assert!(
                Instant::now() < deadline,
                "`{css}` never read `{wanted}` within {secs}s (it read {last:?}); the window \
                 shows:\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Waits up to `secs` for an element matching `css` whose text contains `text`.
    pub fn wait_item(&self, css: &str, text: &str, secs: u64) -> Element {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let found = self.post(
                "/execute/sync",
                &json!({
                    "script": "return [...document.querySelectorAll(arguments[0])]\
                               .find(e => e.innerText.includes(arguments[1])) || null",
                    "args": [css, text],
                }),
            );
            if let Some(id) = found["value"][ELEMENT].as_str() {
                return Element(id.to_owned());
            }
            assert!(
                Instant::now() < deadline,
                "no `{css}` read `{text}` within {secs}s; the window shows:\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Double-clicks `e`.
    pub fn double_click(&self, e: &Element) {
        self.script_on(
            e,
            "arguments[0].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))",
        );
    }

    /// Waits up to `secs` until `script` returns true.
    pub fn wait_script(&self, script: &str, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while self.script(script) != Value::Bool(true) {
            assert!(
                Instant::now() < deadline,
                "`{script}` stayed false for {secs}s"
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Clicks `e`.
    pub fn click(&self, e: &Element) {
        let reply = self.post(&format!("/element/{}/click", e.0), &json!({}));
        assert!(reply["value"].is_null(), "click failed: {reply}");
    }

    /// Clicks the first match of `css`, waiting up to 10 s for it.
    pub fn click_css(&self, css: &str) {
        let e = self.wait(css, 10);
        self.click(&e);
    }

    /// Empties the text field `e`.
    pub fn clear(&self, e: &Element) {
        let reply = self.post(&format!("/element/{}/clear", e.0), &json!({}));
        assert!(reply["value"].is_null(), "clearing failed: {reply}");
    }

    /// Types `text` into `e`; it may hold [`key`] characters.
    pub fn keys(&self, e: &Element, text: &str) {
        let reply = self.post(&format!("/element/{}/value", e.0), &json!({ "text": text }));
        assert!(reply["value"].is_null(), "typing failed: {reply}");
    }

    /// The rendered text of `e`, as the page's `innerText` gives it.
    pub fn text(&self, e: &Element) -> String {
        self.post(
            "/execute/sync",
            &json!({ "script": "return arguments[0].innerText", "args": [{ ELEMENT: e.0 }] }),
        )["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Runs `script` in the window and returns its result.
    pub fn script(&self, script: &str) -> Value {
        self.post("/execute/sync", &json!({ "script": script, "args": [] }))["value"].clone()
    }

    /// Runs `script` in the window with `e` as `arguments[0]`.
    pub fn script_on(&self, e: &Element, script: &str) -> Value {
        self.post(
            "/execute/sync",
            &json!({ "script": script, "args": [{ ELEMENT: e.0 }] }),
        )["value"]
            .clone()
    }

    /// Everything the window shows as text.
    pub fn page_text(&self) -> String {
        self.script("return document.body.innerText")
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// For failure messages: the window's text, whether its WebAssembly compiles here, and the
    /// end of the app's log. A blank window most often means the interface never started.
    fn report(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(40)..].join("\n");
        let wasm = self.post(
            "/execute/async",
            &json!({ "script": WASM_CHECK, "args": [] }),
        )["value"]
            .to_string();
        format!(
            "{}\n--- the interface's WebAssembly ---\n{wasm}\n--- the end of the app's log ---\n{tail}",
            self.page_text()
        )
    }

    /// Saves a PNG of the window to `path`.
    pub fn screenshot(&self, path: &Path) {
        use base64::Engine;
        let data = self.get("/screenshot")["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let png = base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("a base64 screenshot");
        std::fs::write(path, png).expect("write the screenshot");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.agent.delete(&self.url).call();
    }
}
