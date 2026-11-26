//! `cargo xtask latest-json <version> <assets-dir> <base-url>`: the update feed for a release.
//!
//! The app's updater reads `latest.json` from the latest release. It names, for each platform,
//! the package to install and its minisign signature, which the release build writes next to
//! each package (`<file>.sig`). Packages without a signature are left out: the updater would
//! refuse them anyway.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::Path;

/// Which file serves which updater platform key, by how the bundler names it. The first key of
/// each target is the one an install without a known package type falls back to.
const PLATFORMS: &[(&str, &[&str])] = &[
    ("linux-x86_64", &["_amd64.AppImage"]),
    ("linux-x86_64-appimage", &["_amd64.AppImage"]),
    ("linux-x86_64-deb", &["_amd64.deb"]),
    ("linux-x86_64-rpm", &[".x86_64.rpm"]),
    ("linux-aarch64", &["_aarch64.AppImage"]),
    ("linux-aarch64-appimage", &["_aarch64.AppImage"]),
    ("linux-aarch64-deb", &["_arm64.deb"]),
    ("linux-aarch64-rpm", &[".aarch64.rpm"]),
    ("darwin-x86_64", &["_universal.app.tar.gz"]),
    ("darwin-aarch64", &["_universal.app.tar.gz"]),
    ("windows-x86_64", &["_x64-setup.exe"]),
    ("windows-x86_64-nsis", &["_x64-setup.exe"]),
    ("windows-x86_64-msi", &["_x64_en-US.msi"]),
];

pub fn main(args: &[String]) -> Result<()> {
    let [version, dir, base] = args else {
        bail!("usage: cargo xtask latest-json <version> <assets-dir> <base-url>");
    };
    let feed = build(version, Path::new(dir), base, &notes())?;
    let out = Path::new(dir).join("latest.json");
    fs::write(&out, serde_json::to_string_pretty(&feed)? + "\n")
        .with_context(|| format!("writing {}", out.display()))?;
    println!("wrote {}", out.display());
    Ok(())
}

/// The release notes the updater shows, from `TREASURY_RELEASE_NOTES` (a file), if set.
fn notes() -> String {
    std::env::var_os("TREASURY_RELEASE_NOTES")
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default()
}

fn build(version: &str, dir: &Path, base: &str, notes: &str) -> Result<Value> {
    let version = version.trim_start_matches('v');
    let mut files: Vec<String> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    files.sort();
    let mut platforms = Map::new();
    for (key, suffixes) in PLATFORMS {
        let package = files.iter().find(|f| {
            suffixes.iter().any(|s| f.ends_with(s))
                && files.contains(&format!("{f}.sig"))
                && f.contains(version)
        });
        let Some(package) = package else { continue };
        let signature = fs::read_to_string(dir.join(format!("{package}.sig")))
            .with_context(|| format!("reading the signature of {package}"))?;
        platforms.insert(
            (*key).to_owned(),
            json!({
                "signature": signature.trim(),
                "url": format!("{}/{package}", base.trim_end_matches('/')),
            }),
        );
    }
    ensure!(
        !platforms.is_empty(),
        "no signed package for version {version} in {}",
        dir.display()
    );
    Ok(json!({
        "version": version,
        "notes": notes.trim(),
        "pub_date": pub_date(),
        "platforms": platforms,
    }))
}

/// Now, as RFC 3339 in UTC, without a date library.
fn pub_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_signed_package_gets_its_platform_keys() {
        let dir = std::env::temp_dir().join(format!("treasury-feed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, sig) in [
            ("Treasury_0.1.0_amd64.AppImage", Some("sig-appimage\n")),
            ("Treasury_0.1.0_amd64.deb", Some("sig-deb")),
            ("Treasury-0.1.0-1.x86_64.rpm", None),
            ("Treasury_0.1.0_universal.app.tar.gz", Some("sig-mac")),
            ("Treasury_0.1.0_x64-setup.exe", Some("sig-nsis")),
            ("Treasury_0.0.9_x64_en-US.msi", Some("old")),
        ] {
            fs::write(dir.join(name), "package").unwrap();
            if let Some(s) = sig {
                fs::write(dir.join(format!("{name}.sig")), s).unwrap();
            }
        }
        let feed = build(
            "v0.1.0",
            &dir,
            "https://example.test/download/v0.1.0/",
            "Notes",
        )
        .unwrap();
        let p = &feed["platforms"];
        assert_eq!(feed["version"], "0.1.0");
        assert_eq!(p["linux-x86_64"]["signature"], "sig-appimage");
        assert_eq!(
            p["linux-x86_64-deb"]["url"],
            "https://example.test/download/v0.1.0/Treasury_0.1.0_amd64.deb"
        );
        // Unsigned, or of another version: left out.
        assert!(p.get("linux-x86_64-rpm").is_none());
        assert!(p.get("windows-x86_64-msi").is_none());
        assert_eq!(p["darwin-aarch64"], p["darwin-x86_64"]);
        assert_eq!(p["windows-x86_64"]["signature"], "sig-nsis");
        assert!(build("0.2.0", &dir, "https://x", "").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dates_are_rfc_3339() {
        let d = pub_date();
        assert_eq!(d.len(), 20, "{d}");
        assert!(d.starts_with("20") && d.ends_with('Z'), "{d}");
    }
}
