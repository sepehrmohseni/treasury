//! `cargo xtask ci`: the checks CI runs, in the order CI runs them.
//!
//! GitHub Actions calls `cargo xtask ci <step>` per job, so a step defined here is the single
//! definition of that check. A step that cannot run on this machine (a missing tool, no Docker)
//! reports itself as skipped, and the summary lists it; a skipped step never counts as passed.

use crate::run::{self, cmd};
use crate::workspace_root;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// What a step did.
enum Outcome {
    Passed,
    Skipped(String),
}

struct Step {
    name: &'static str,
    about: &'static str,
    run: fn(&Path) -> Result<Outcome>,
}

const STEPS: &[Step] = &[
    Step {
        name: "fmt",
        about: "rustfmt check of the whole workspace",
        run: fmt,
    },
    Step {
        name: "clippy",
        about: "clippy on every target with warnings denied",
        run: clippy,
    },
    Step {
        name: "wasm",
        about: "clippy on the interface compiled to WebAssembly, with warnings denied",
        run: wasm,
    },
    Step {
        name: "deny",
        about: "cargo-deny: licenses, advisories, bans and sources",
        run: deny,
    },
    Step {
        name: "audit",
        about: "cargo-audit against the RustSec advisory database",
        run: audit,
    },
    Step {
        name: "test",
        about: "unit tests of every crate (cargo nextest), without containers",
        run: test,
    },
    Step {
        name: "integration",
        about: "integration and conformance tests against containers (needs Docker)",
        run: integration,
    },
    Step {
        name: "desktop",
        about: "the desktop app end to end through tauri-driver (Linux; needs Docker)",
        run: desktop,
    },
];

/// Tests that start containers live in modules named `docker`.
const DOCKER_TESTS: &str = "test(/docker::/)";

/// Tests that drive the built desktop app live in modules named `desktop`.
const DESKTOP_TESTS: &str = "test(/desktop::/)";

/// WebKitGTK's WebDriver, which tauri-driver drives the window through.
const WEBKIT_DRIVER: &str = "/usr/bin/WebKitWebDriver";

/// Entry point for `cargo xtask ci [--list] [STEP...]`.
pub fn main(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--list") {
        for s in STEPS {
            println!("{:<12} {}", s.name, s.about);
        }
        return Ok(());
    }
    let selected: Vec<&Step> = if args.is_empty() {
        STEPS.iter().collect()
    } else {
        let mut v = Vec::new();
        for a in args {
            match STEPS.iter().find(|s| s.name == a) {
                Some(s) => v.push(s),
                None => bail!("unknown step `{a}`; `cargo xtask ci --list` lists the steps"),
            }
        }
        v
    };

    let root = workspace_root();
    let mut summary: Vec<(&str, String)> = Vec::new();
    let mut failure = None;
    for step in selected {
        println!("\n\x1b[1m==> {}\x1b[0m  {}", step.name, step.about);
        let started = Instant::now();
        match (step.run)(&root) {
            Ok(Outcome::Passed) => summary.push((
                step.name,
                format!("passed in {:.1}s", started.elapsed().as_secs_f64()),
            )),
            Ok(Outcome::Skipped(why)) => summary.push((step.name, format!("SKIPPED: {why}"))),
            Err(err) => {
                summary.push((step.name, "FAILED".to_owned()));
                failure = Some((step.name, err));
                break;
            }
        }
    }

    println!("\n\x1b[1mSummary\x1b[0m");
    for (name, result) in &summary {
        println!("  {name:<12} {result}");
    }
    let skipped = summary
        .iter()
        .filter(|(_, r)| r.starts_with("SKIPPED"))
        .count();
    match failure {
        Some((name, err)) => bail!("step `{name}` failed: {err:#}"),
        None if skipped > 0 => {
            println!("\x1b[1;33mPassed, with {skipped} step(s) skipped.\x1b[0m");
            Ok(())
        }
        None => {
            println!("\x1b[1;32mAll checks passed.\x1b[0m");
            Ok(())
        }
    }
}

fn cargo(root: &Path, args: &[&str]) -> Result<Outcome> {
    run::status(&mut cmd(root, "cargo", args))?;
    Ok(Outcome::Passed)
}

fn fmt(root: &Path) -> Result<Outcome> {
    cargo(root, &["fmt", "--all", "--check"])
}

fn clippy(root: &Path) -> Result<Outcome> {
    cargo(
        root,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )
}

fn wasm(root: &Path) -> Result<Outcome> {
    let sysroot = run::output(&mut cmd(root, "rustc", ["--print", "sysroot"]))?;
    let target = Path::new(&sysroot)
        .join("lib")
        .join("rustlib")
        .join("wasm32-unknown-unknown");
    if !target.is_dir() {
        return Ok(Outcome::Skipped(
            "the wasm32-unknown-unknown target is not installed \
             (rustup target add wasm32-unknown-unknown)"
                .into(),
        ));
    }
    cargo(
        root,
        &[
            "clippy",
            "-p",
            "treasury-ui",
            "--target",
            "wasm32-unknown-unknown",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )
}

fn deny(root: &Path) -> Result<Outcome> {
    if !run::exists("cargo-deny", "--version") {
        return Ok(Outcome::Skipped("cargo-deny is not installed".into()));
    }
    cargo(root, &["deny", "--locked", "check"])
}

fn audit(root: &Path) -> Result<Outcome> {
    if !run::exists("cargo-audit", "--version") {
        return Ok(Outcome::Skipped("cargo-audit is not installed".into()));
    }
    // cargo-deny covers the same database and also decides about unmaintained crates (deny.toml);
    // cargo-audit reads the lockfile alone, which is what downstream packagers run, so both stay.
    let lock: PathBuf = root.join("Cargo.lock");
    cargo(root, &["audit", "--file", &lock.to_string_lossy()])
}

fn test(root: &Path) -> Result<Outcome> {
    if !run::exists("cargo-nextest", "--version") {
        return Ok(Outcome::Skipped("cargo-nextest is not installed".into()));
    }
    cargo(
        root,
        &[
            "nextest",
            "run",
            "--workspace",
            "--locked",
            "--profile",
            "ci",
            "--no-tests",
            "warn",
            "-E",
            &format!("not ({DOCKER_TESTS} | {DESKTOP_TESTS})"),
        ],
    )
}

fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some()
        || std::env::var_os("TREASURY_DOCKER").is_some()
        || std::path::Path::new("/var/run/docker.sock").exists()
}

fn integration(root: &Path) -> Result<Outcome> {
    if !run::exists("cargo-nextest", "--version") {
        return Ok(Outcome::Skipped("cargo-nextest is not installed".into()));
    }
    if !docker_available() {
        return Ok(Outcome::Skipped("Docker is not available".into()));
    }
    cargo(
        root,
        &[
            "nextest",
            "run",
            "--workspace",
            "--locked",
            "--profile",
            "integration",
            "-E",
            DOCKER_TESTS,
        ],
    )
}

fn desktop(root: &Path) -> Result<Outcome> {
    if !cfg!(target_os = "linux") {
        return Ok(Outcome::Skipped("the desktop test runs on Linux".into()));
    }
    for (tool, probe) in [
        ("cargo-nextest", "--version"),
        ("cargo-tauri", "--version"),
        ("trunk", "--version"),
        ("tauri-driver", "--help"),
    ] {
        if !run::exists(tool, probe) {
            return Ok(Outcome::Skipped(format!("{tool} is not installed")));
        }
    }
    if !Path::new(WEBKIT_DRIVER).exists() {
        return Ok(Outcome::Skipped("WebKitWebDriver is not installed".into()));
    }
    if !docker_available() {
        return Ok(Outcome::Skipped("Docker is not available".into()));
    }
    for tool in ["dbus-daemon", "dbus-send", "gnome-keyring-daemon"] {
        if !on_path(tool) {
            return Ok(Outcome::Skipped(format!("{tool} is not installed")));
        }
    }

    // The app with its interface built in, as a release has it, but with debug code generation.
    run::status(&mut cmd(
        &root.join("crates").join("app"),
        "cargo",
        ["tauri", "build", "--debug", "--no-bundle", "--ci"],
    ))?;
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let app = target.join("debug").join("treasury-app");

    let mut test = cmd(
        root,
        "cargo",
        [
            "nextest",
            "run",
            "-p",
            "treasury-tests",
            "--test",
            "desktop",
            "--locked",
            "--profile",
            "integration",
            "-E",
            DESKTOP_TESTS,
        ],
    );
    test.env("TREASURY_APP", &app)
        .env("TREASURY_NATIVE_DRIVER", WEBKIT_DRIVER)
        // Software rendering: containers and CI machines have no usable GPU buffers.
        .env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    // A display: the desktop's own, or a virtual one for this run.
    let mut xvfb = None;
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        if !on_path("Xvfb") {
            return Ok(Outcome::Skipped(
                "no display, and Xvfb is not installed".into(),
            ));
        }
        let display = ":97";
        let child = std::process::Command::new("Xvfb")
            .args([display, "-screen", "0", "1400x900x24", "-nolisten", "tcp"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        xvfb = Some(child);
        std::thread::sleep(std::time::Duration::from_secs(1));
        test.env("DISPLAY", display);
    }
    let keyring = Keyring::start()?;
    test.env("DBUS_SESSION_BUS_ADDRESS", &keyring.address);
    let result = run::status(&mut test);
    drop(keyring);
    if let Some(mut x) = xvfb {
        let _ = x.kill();
        let _ = x.wait();
    }
    result.map(|()| Outcome::Passed)
}

/// A session bus of its own with an unlocked GNOME Keyring on it, for the app to keep its keys
/// in as it does on a desktop, so the test can restart the app and find its tabs and history.
/// The private bus keeps the test away from any keyring the machine's own desktop has.
struct Keyring {
    dir: PathBuf,
    address: String,
    bus: std::process::Child,
    daemon: std::process::Child,
}

impl Keyring {
    fn start() -> Result<Self> {
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("treasury-keyring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let runtime = dir.join("run");
        std::fs::create_dir_all(&runtime)?;
        std::fs::create_dir_all(dir.join("data"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
        }
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(format!("--address={address}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        match Self::daemon(&dir, &socket, &address) {
            Ok(daemon) => Ok(Self {
                dir,
                address,
                bus,
                daemon,
            }),
            Err(e) => {
                let _ = bus.kill();
                let _ = bus.wait();
                let _ = std::fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }

    /// Starts gnome-keyring on the bus at `address` and waits for its Secret Service.
    fn daemon(dir: &Path, socket: &Path, address: &str) -> Result<std::process::Child> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        if !wait_for(Duration::from_secs(10), || socket.exists()) {
            bail!("the session bus did not start");
        }
        let mut child = Command::new("gnome-keyring-daemon")
            .args(["--foreground", "--unlock", "--components=secrets"])
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // The password of this throwaway keyring, which only ever holds the test's keys. With an
        // empty one, gnome-keyring creates no login keyring to keep them in.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b"treasury-desktop-test")?;
        }
        let ready = wait_for(Duration::from_secs(15), || {
            Command::new("dbus-send")
                .env("DBUS_SESSION_BUS_ADDRESS", address)
                .args([
                    "--session",
                    "--print-reply",
                    "--dest=org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus.NameHasOwner",
                    "string:org.freedesktop.secrets",
                ])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("boolean true"))
        });
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            bail!("gnome-keyring did not start its Secret Service");
        }
        Ok(child)
    }
}

impl Drop for Keyring {
    fn drop(&mut self) {
        for child in [&mut self.daemon, &mut self.bus] {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Whether `ready` turns true within `limit`, asking every 100 ms.
fn wait_for(limit: std::time::Duration, mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Whether `program` is in a directory on `PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}
