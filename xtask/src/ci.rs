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
    let result = run::status(&mut test);
    if let Some(mut x) = xvfb {
        let _ = x.kill();
        let _ = x.wait();
    }
    result.map(|()| Outcome::Passed)
}

/// Whether `program` is in a directory on `PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}
