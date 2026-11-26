//! Running external commands with a visible header and a clear error on failure.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// Builds a command for `program` with `args`, run from `dir`.
pub fn cmd<I, S>(dir: &Path, program: &str, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = Command::new(program);
    c.args(args).current_dir(dir);
    c
}

/// Runs `c`, streaming its output, and fails unless it exits successfully.
pub fn status(c: &mut Command) -> Result<()> {
    println!("\x1b[2m$ {}\x1b[0m", display(c));
    let status = c
        .status()
        .with_context(|| format!("could not start `{}`", display(c)))?;
    if !status.success() {
        bail!("`{}` failed with {status}", display(c));
    }
    Ok(())
}

/// Runs `c` quietly and returns its trimmed standard output.
pub fn output(c: &mut Command) -> Result<String> {
    let out = c
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("could not start `{}`", display(c)))?;
    if !out.status.success() {
        bail!("`{}` failed with {}", display(c), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// True when `program` can be started (used to report missing tools as skipped steps).
pub fn exists(program: &str, probe_arg: &str) -> bool {
    Command::new(program)
        .arg(probe_arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn display(c: &Command) -> String {
    let mut s = c.get_program().to_string_lossy().into_owned();
    for a in c.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}
