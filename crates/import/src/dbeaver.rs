//! DBeaver: `data-sources.json` plus the encrypted `credentials-config.json` beside it, read from
//! a project export (`.dbp`, a zip archive), a zip holding one, a workspace or project folder, or
//! the JSON file itself.

use crate::crypto::dbeaver_credentials;
use crate::{Draft, Imported, Report, Skipped, apply_uri, is_local, unreadable};
use secrecy::SecretString;
use serde_json::{Map, Value as J};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use treasury_core::Result;
use treasury_core::config::{Engine, Environment, SecretSlot, SshAuth, SshConfig, SshHop};

/// Archive entries larger than this are not read (a defence against zip bombs).
const MAX_ENTRY: u64 = 64 * 1024 * 1024;

/// One project's data sources and its credentials.
struct Project {
    data_sources: Vec<J>,
    credentials: Option<J>,
}

pub fn import(path: &Path) -> Result<Report> {
    let projects = load(path)?;
    if projects.is_empty() {
        return Err(unreadable("DBeaver", "it holds no data-sources.json"));
    }
    let mut report = Report::default();
    for p in projects {
        for ds in &p.data_sources {
            convert(ds, p.credentials.as_ref(), &mut report);
        }
    }
    Ok(report)
}

/// The `.dbeaver` folders below `dir` (a workspace holds one per project).
pub fn find_in_dir(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut found = Vec::new();
    walk(dir, 0, &mut found);
    (!found.is_empty()).then_some(found)
}

fn walk(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let candidate = dir.join(".dbeaver");
    if candidate.join("data-sources.json").exists() {
        found.push(candidate);
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            let hidden = p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if p.is_dir() && !hidden {
                walk(&p, depth + 1, found);
            }
        }
    }
}

fn read_json(bytes: &[u8]) -> Result<J> {
    serde_json::from_slice(bytes).map_err(|e| unreadable("DBeaver", e))
}

fn credentials_from(bytes: &[u8]) -> Option<J> {
    let plain = dbeaver_credentials(bytes)?;
    serde_json::from_slice(&plain).ok()
}

fn load(path: &Path) -> Result<Vec<Project>> {
    if path.is_dir() {
        let mut out = Vec::new();
        for dir in find_in_dir(path).unwrap_or_default() {
            out.push(project_from_dir(&dir)?);
        }
        return Ok(out);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".json") {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let ds = read_json(&std::fs::read(path).map_err(|e| unreadable("DBeaver", e))?)?;
        let credentials = std::fs::read(dir.join("credentials-config.json"))
            .ok()
            .and_then(|b| credentials_from(&b));
        return Ok(vec![Project {
            data_sources: vec![ds],
            credentials,
        }]);
    }
    let bytes = std::fs::read(path).map_err(|e| unreadable("DBeaver", e))?;
    from_archive(&bytes, 0)
}

fn project_from_dir(dir: &Path) -> Result<Project> {
    let mut data_sources = Vec::new();
    let mut credentials = None;
    for e in std::fs::read_dir(dir)
        .map_err(|e| unreadable("DBeaver", e))?
        .flatten()
    {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("data-sources") && n.ends_with(".json") {
            data_sources.push(read_json(
                &std::fs::read(e.path()).map_err(|e| unreadable("DBeaver", e))?,
            )?);
        } else if n.starts_with("credentials-config") && n.ends_with(".json") {
            credentials = std::fs::read(e.path())
                .ok()
                .and_then(|b| credentials_from(&b));
        }
    }
    Ok(Project {
        data_sources,
        credentials,
    })
}

/// Reads every project in a zip archive, descending into one nested `.dbp` or `.zip`.
fn from_archive(bytes: &[u8], depth: u32) -> Result<Vec<Project>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| unreadable("DBeaver", format!("the archive cannot be opened ({e})")))?;
    let mut by_dir: BTreeMap<String, (Vec<J>, Option<J>)> = BTreeMap::new();
    let mut nested = Vec::new();
    for i in 0..zip.len() {
        let mut entry = match zip.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.is_dir() || entry.size() > MAX_ENTRY {
            continue;
        }
        let Ok(raw_name) = entry.name() else { continue };
        let name = raw_name.replace('\\', "/");
        let (dir, file) = name.rsplit_once('/').unwrap_or(("", name.as_str()));
        let lower = file.to_ascii_lowercase();
        let wanted = (lower.starts_with("data-sources") && lower.ends_with(".json"))
            || (lower.starts_with("credentials-config") && lower.ends_with(".json"))
            || (depth == 0 && (lower.ends_with(".dbp") || lower.ends_with(".zip")));
        if !wanted {
            continue;
        }
        let mut buf = Vec::new();
        (&mut entry)
            .take(MAX_ENTRY)
            .read_to_end(&mut buf)
            .map_err(|e| unreadable("DBeaver", e))?;
        if lower.ends_with(".dbp") || lower.ends_with(".zip") {
            nested.push(buf);
        } else if lower.starts_with("data-sources") {
            by_dir
                .entry(dir.to_owned())
                .or_default()
                .0
                .push(read_json(&buf)?);
        } else {
            by_dir.entry(dir.to_owned()).or_default().1 = credentials_from(&buf);
        }
    }
    let mut out: Vec<Project> = by_dir
        .into_values()
        .filter(|(ds, _)| !ds.is_empty())
        .map(|(data_sources, credentials)| Project {
            data_sources,
            credentials,
        })
        .collect();
    for inner in nested {
        out.extend(from_archive(&inner, depth + 1)?);
    }
    Ok(out)
}

fn text(obj: &Map<String, J>, key: &str) -> Option<String> {
    match obj.get(key)? {
        J::String(s) if !s.is_empty() => Some(s.clone()),
        J::Number(n) => Some(n.to_string()),
        J::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn port(obj: &Map<String, J>, key: &str) -> Option<u16> {
    text(obj, key)?.trim().parse().ok()
}

fn engine(provider: &str, driver: &str) -> Option<Engine> {
    let p = provider.to_ascii_lowercase();
    let d = driver.to_ascii_lowercase();
    let either = |s: &str| p.contains(s) || d.contains(s);
    Some(if either("mariadb") {
        Engine::MariaDb
    } else if either("mysql") {
        Engine::MySql
    } else if either("postgres") {
        Engine::Postgres
    } else if either("sqlserver")
        || either("mssql")
        || d.contains("microsoft")
        || d.contains("jtds")
    {
        Engine::SqlServer
    } else if either("oracle") {
        Engine::Oracle
    } else if either("db2") {
        Engine::Db2
    } else if either("mongo") {
        Engine::MongoDb
    } else if either("valkey") {
        Engine::Valkey
    } else if either("redis") {
        Engine::Redis
    } else if either("scylla") {
        Engine::ScyllaDb
    } else if either("cassandra") {
        Engine::Cassandra
    } else if either("snowflake") {
        Engine::Snowflake
    } else if either("databricks") {
        Engine::Databricks
    } else {
        return None;
    })
}

fn environment(kind: Option<&str>, host: Option<&str>) -> Environment {
    match kind.map(str::to_ascii_lowercase).as_deref() {
        Some(k) if k.contains("prod") => Environment::Production,
        Some(k) if k.contains("test") || k.contains("stag") || k.contains("qa") => {
            Environment::Staging
        }
        _ if host.is_some_and(is_local) => Environment::Local,
        _ => Environment::Development,
    }
}

fn hop(props: &Map<String, J>, prefix: &str, user: Option<String>) -> Option<SshHop> {
    let host = text(props, &format!("{prefix}host"))?;
    let port = port(props, &format!("{prefix}port")).unwrap_or(22);
    let auth = match text(props, &format!("{prefix}authType")).as_deref() {
        Some("PUBLIC_KEY") => match text(props, &format!("{prefix}keyPath")) {
            Some(p) => SshAuth::KeyFile {
                path: PathBuf::from(p),
            },
            None => SshAuth::Agent,
        },
        Some("AGENT") => SshAuth::Agent,
        _ => SshAuth::Password,
    };
    Some(SshHop {
        host,
        port,
        user: user.unwrap_or_else(|| "root".into()),
        auth,
    })
}

fn convert(ds_file: &J, credentials: Option<&J>, report: &mut Report) {
    let Some(connections) = ds_file.get("connections").and_then(J::as_object) else {
        return;
    };
    for (id, conn) in connections {
        let Some(conn) = conn.as_object() else {
            continue;
        };
        let name = text(conn, "name").unwrap_or_else(|| id.clone());
        let provider = text(conn, "provider").unwrap_or_default();
        let driver = text(conn, "driver").unwrap_or_default();
        let Some(engine) = engine(&provider, &driver) else {
            report.skipped.push(Skipped {
                name,
                reason: format!(
                    "Treasury does not support the \"{}\" driver.",
                    if driver.is_empty() { provider } else { driver }
                ),
            });
            continue;
        };
        let empty = Map::new();
        let cfg = conn
            .get("configuration")
            .and_then(J::as_object)
            .unwrap_or(&empty);
        let creds = credentials.and_then(|c| c.get(id)).and_then(J::as_object);
        let main = creds
            .and_then(|c| c.get("#connection"))
            .and_then(J::as_object);
        let host = text(cfg, "host");
        let mut d = Draft::new(&name, engine);
        d.port = port(cfg, "port");
        d.database = text(cfg, "database");
        d.user = main
            .and_then(|m| text(m, "user"))
            .or_else(|| text(cfg, "user"));
        d.environment = environment(text(cfg, "type").as_deref(), host.as_deref());
        d.folder = text(conn, "folder");
        d.read_only = conn.get("read-only").and_then(J::as_bool).unwrap_or(false);
        d.host = host.clone();
        let mut config = match d.build() {
            Ok(c) => c,
            Err(e) => {
                report.skipped.push(Skipped {
                    name,
                    reason: e.message.clone(),
                });
                continue;
            }
        };
        let mut notes = Vec::new();
        let mut secrets = Vec::new();
        if host.is_none()
            && let Some(url) = text(cfg, "url")
            && let Some(pw) = apply_uri(&mut config, &url)
        {
            secrets.push((SecretSlot::Password, pw));
        }
        let saved = conn
            .get("save-password")
            .and_then(J::as_bool)
            .unwrap_or(false);
        match main
            .and_then(|m| text(m, "password"))
            .or_else(|| text(cfg, "password"))
        {
            Some(pw) => secrets.push((SecretSlot::Password, SecretString::from(pw))),
            None if saved && credentials.is_none() => {
                notes.push("The saved password was not found; DBeaver keeps it in credentials-config.json.".into());
            }
            None => {}
        }
        if let Some(handlers) = cfg.get("handlers").and_then(J::as_object) {
            for (hid, h) in handlers {
                let Some(h) = h.as_object() else { continue };
                if !h.get("enabled").and_then(J::as_bool).unwrap_or(false) {
                    continue;
                }
                if hid != "ssh_tunnel" {
                    notes.push(format!("The \"{hid}\" network setting was not imported."));
                    continue;
                }
                let props = h.get("properties").and_then(J::as_object).unwrap_or(&empty);
                let net = creds
                    .and_then(|c| c.get("network/ssh_tunnel"))
                    .and_then(J::as_object);
                let user = net
                    .and_then(|n| text(n, "user"))
                    .or_else(|| text(h, "user"));
                let Some(target) = hop(props, "", user) else {
                    notes.push("The SSH tunnel has no host and was not imported.".into());
                    continue;
                };
                let jump = if text(props, "jumpServer0.enabled").as_deref() == Some("true") {
                    hop(props, "jumpServer0.", text(props, "jumpServer0.name"))
                } else {
                    None
                };
                if text(props, "jumpServer1.enabled").as_deref() == Some("true") {
                    notes.push(
                        "Only the first SSH jump server was imported; Treasury supports one."
                            .into(),
                    );
                }
                if let Some(pw) = net.and_then(|n| text(n, "password")) {
                    let slot = if matches!(target.auth, SshAuth::Password) {
                        SecretSlot::SshPassword
                    } else {
                        SecretSlot::SshKeyPassphrase
                    };
                    secrets.push((slot, SecretString::from(pw)));
                }
                if let Some(pw) = net.and_then(|n| text(n, "jumpServer0.password")) {
                    secrets.push((SecretSlot::JumpPassword, SecretString::from(pw)));
                }
                config.ssh = Some(SshConfig { target, jump });
            }
        }
        report.imported.push(Imported {
            config,
            secrets,
            notes,
        });
    }
}
