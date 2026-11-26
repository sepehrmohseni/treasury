//! `cargo xtask release <version>` and `cargo xtask check-version <tag>`.
//!
//! The `[workspace.package] version` in Cargo.toml is the single source of truth. A release copies
//! it into every other manifest that carries a version, regenerates CHANGELOG.md from the
//! Conventional Commits since the last tag, commits `chore(release): <version>` and creates the
//! annotated tag `v<version>`. Pushing stays a separate, deliberate step.

use crate::run::{self, cmd};
use crate::workspace_root;
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use std::fs;
use std::path::Path;

/// JSON manifests whose top-level `"version"` must equal the workspace version.
const JSON_MANIFESTS: &[&str] = &[];

/// Entry point for `cargo xtask release <version>`.
pub fn release(args: &[String]) -> Result<()> {
    let [version] = args else {
        bail!("usage: cargo xtask release <version>");
    };
    let next = Version::parse(version.trim_start_matches('v'))
        .with_context(|| format!("`{version}` is not a SemVer version"))?;
    let root = workspace_root();

    let current = workspace_version(&root)?;
    ensure!(
        next > current,
        "{next} is not newer than the current version {current}"
    );
    let dirty = run::output(&mut cmd(&root, "git", ["status", "--porcelain"]))?;
    ensure!(
        dirty.is_empty(),
        "the working tree has uncommitted changes:\n{dirty}"
    );
    let branch = run::output(&mut cmd(&root, "git", ["branch", "--show-current"]))?;
    ensure!(
        branch == "main",
        "releases are cut from main, not `{branch}`"
    );
    let tag = format!("v{next}");
    let existing = run::output(&mut cmd(&root, "git", ["tag", "--list", &tag]))?;
    ensure!(existing.is_empty(), "tag {tag} already exists");

    set_workspace_version(&root, &next)?;
    for manifest in JSON_MANIFESTS {
        set_json_version(&root.join(manifest), &next)?;
    }
    // Rewrite the lockfile entries of the workspace crates without touching dependencies.
    run::status(&mut cmd(
        &root,
        "cargo",
        ["update", "--workspace", "--offline"],
    ))?;
    run::status(&mut cmd(
        &root,
        "git-cliff",
        [
            "--config",
            "cliff.toml",
            "--tag",
            &tag,
            "--output",
            "CHANGELOG.md",
        ],
    ))?;

    let mut add = vec!["add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md"];
    add.extend_from_slice(JSON_MANIFESTS);
    run::status(&mut cmd(&root, "git", &add))?;
    let message = format!("chore(release): {next}");
    run::status(&mut cmd(
        &root,
        "git",
        ["commit", "--quiet", "-m", &message],
    ))?;
    let tag_message = format!("Treasury {next}");
    run::status(&mut cmd(
        &root,
        "git",
        ["tag", "-a", &tag, "-m", &tag_message],
    ))?;

    println!(
        "\nCommitted `{message}` and tagged {tag}. Publish with:\n  git push origin main {tag}"
    );
    Ok(())
}

/// Entry point for `cargo xtask check-version <tag>`: the release workflow's first gate.
pub fn check_version(args: &[String]) -> Result<()> {
    let [tag] = args else {
        bail!("usage: cargo xtask check-version <tag>");
    };
    let Some(raw) = tag.strip_prefix('v') else {
        bail!("tag `{tag}` does not start with `v`");
    };
    let tagged = Version::parse(raw).with_context(|| format!("tag `{tag}` is not vX.Y.Z"))?;
    let root = workspace_root();

    let mut mismatches = Vec::new();
    let workspace = workspace_version(&root)?;
    if workspace != tagged {
        mismatches.push(format!("Cargo.toml has {workspace}"));
    }
    for manifest in JSON_MANIFESTS {
        let found = json_version(&root.join(manifest))?;
        if found != tagged {
            mismatches.push(format!("{manifest} has {found}"));
        }
    }
    ensure!(
        mismatches.is_empty(),
        "tag {tag} does not match: {}",
        mismatches.join(", ")
    );
    println!(
        "{tag} matches Cargo.toml and {} other manifest(s)",
        JSON_MANIFESTS.len()
    );
    Ok(())
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn workspace_version(root: &Path) -> Result<Version> {
    let doc: toml_edit::DocumentMut = read(&root.join("Cargo.toml"))?.parse()?;
    let raw = doc["workspace"]["package"]["version"]
        .as_str()
        .context("Cargo.toml has no [workspace.package] version")?;
    Ok(Version::parse(raw)?)
}

fn set_workspace_version(root: &Path, version: &Version) -> Result<()> {
    let path = root.join("Cargo.toml");
    let mut doc: toml_edit::DocumentMut = read(&path)?.parse()?;
    doc["workspace"]["package"]["version"] = toml_edit::value(version.to_string());
    fs::write(&path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

fn json_version(path: &Path) -> Result<Version> {
    let value: serde_json::Value = serde_json::from_str(&read(path)?)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let raw = value["version"]
        .as_str()
        .with_context(|| format!("{} has no top-level \"version\"", path.display()))?;
    Ok(Version::parse(raw)?)
}

/// Replaces the top-level `"version"` value in place, keeping the file's formatting.
fn set_json_version(path: &Path, version: &Version) -> Result<()> {
    let old = json_version(path)?;
    let text = read(path)?;
    let needle = format!("\"version\": \"{old}\"");
    ensure!(
        text.matches(&needle).count() == 1,
        "{} must contain `{needle}` exactly once",
        path.display()
    );
    let updated = text.replacen(&needle, &format!("\"version\": \"{version}\""), 1);
    fs::write(path, &updated).with_context(|| format!("writing {}", path.display()))?;
    ensure!(
        json_version(path)? == *version,
        "{} did not take the new version",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("package.json");
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn set_json_version_keeps_formatting() {
        let path = scratch(
            "keep",
            "{\n  \"name\": \"ui\",\n  \"version\": \"0.0.0\",\n  \"dependencies\": {\n    \"x\": \"1.0.0\"\n  }\n}\n",
        );
        set_json_version(&path, &Version::new(0, 1, 0)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "{\n  \"name\": \"ui\",\n  \"version\": \"0.1.0\",\n  \"dependencies\": {\n    \"x\": \"1.0.0\"\n  }\n}\n"
        );
    }

    #[test]
    fn set_json_version_refuses_ambiguous_files() {
        let path = scratch(
            "ambiguous",
            "{\"version\": \"0.0.0\", \"nested\": {\"version\": \"0.0.0\"}}",
        );
        assert!(set_json_version(&path, &Version::new(0, 1, 0)).is_err());
    }

    #[test]
    fn json_version_requires_a_top_level_version() {
        let path = scratch("missing", "{\"name\": \"ui\"}");
        assert!(json_version(&path).is_err());
    }

    #[test]
    fn check_version_rejects_malformed_tags() {
        assert!(check_version(&["0.1.0".to_owned()]).is_err());
        assert!(check_version(&["vnext".to_owned()]).is_err());
    }
}
