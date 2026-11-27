//! Workspace tasks: `cargo xtask ci`, `cargo xtask release <version>`,
//! `cargo xtask check-version <tag>`, `cargo xtask latest-json ...`.
//!
//! The same steps run locally (usually through `scripts/docker-check.sh`) and in GitHub Actions,
//! so a green local run means the same commands pass in CI.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod ci;
mod feed;
mod release;
mod run;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <task> [args]

tasks:
  ci [--list] [STEP...]     run every check CI runs, or only the named steps
  release <version>         bump every manifest, write CHANGELOG.md, commit and tag
  check-version <tag>       fail unless <tag> (vX.Y.Z) matches every manifest version
  latest-json <version> <dir> <base-url>
                            write the updater feed for the signed packages in <dir>
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("ci") => ci::main(&args[1..]),
        Some("release") => release::release(&args[1..]),
        Some("latest-json") => feed::main(&args[1..]),
        Some("check-version") => release::check_version(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown task `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The workspace root: the parent of this crate's manifest directory.
///
/// `cargo run` sets the variable at run time too, and that value wins: cargo can reuse a binary
/// built from a copy of the checkout elsewhere (the Docker image builds in `/work`, while
/// `scripts/docker-check.sh release` runs on the real checkout), and the compiled-in path would
/// then point at the copy.
fn workspace_root() -> PathBuf {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    manifest.parent().map(PathBuf::from).unwrap_or(manifest)
}
