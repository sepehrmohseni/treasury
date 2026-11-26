//! pgAdmin 4 `servers.json` exports (File > Export Servers). pgAdmin does not export passwords.

use crate::{Draft, Imported, Report, is_local, unreadable};
use serde_json::{Map, Value as J};
use std::path::{Path, PathBuf};
use treasury_core::Result;
use treasury_core::config::{Engine, Environment, SshAuth, SshConfig, SshHop, TlsMode};

fn text(o: &Map<String, J>, k: &str) -> Option<String> {
    match o.get(k)? {
        J::String(s) if !s.is_empty() => Some(s.clone()),
        J::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn num(o: &Map<String, J>, k: &str) -> Option<i64> {
    match o.get(k)? {
        J::Number(n) => n.as_i64(),
        J::String(s) => s.parse().ok(),
        J::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

pub fn import(path: &Path) -> Result<Report> {
    let bytes = std::fs::read(path).map_err(|e| unreadable("pgAdmin", e))?;
    let doc: J = serde_json::from_slice(&bytes).map_err(|e| unreadable("pgAdmin", e))?;
    let servers = doc
        .get("Servers")
        .and_then(J::as_object)
        .ok_or_else(|| unreadable("pgAdmin", "there is no \"Servers\" object"))?;
    let mut report = Report::default();
    for s in servers.values().filter_map(J::as_object) {
        let empty = Map::new();
        let params = s
            .get("ConnectionParameters")
            .and_then(J::as_object)
            .unwrap_or(&empty);
        let host = text(s, "Host");
        let mut d = Draft::new(
            &text(s, "Name").unwrap_or_else(|| "pgAdmin server".into()),
            Engine::Postgres,
        );
        d.port = num(s, "Port").and_then(|p| u16::try_from(p).ok());
        d.database = text(s, "MaintenanceDB");
        d.user = text(s, "Username");
        d.folder = text(s, "Group").filter(|g| g != "Servers");
        d.environment = if host.as_deref().is_some_and(is_local) {
            Environment::Local
        } else {
            Environment::Development
        };
        d.host = host;
        let mut config = d.build()?;
        let mut notes =
            vec!["pgAdmin does not export passwords; enter it before connecting.".to_owned()];
        let sslmode = text(params, "sslmode").or_else(|| text(s, "SSLMode"));
        match sslmode.as_deref() {
            Some("disable") => config.tls.mode = TlsMode::Disable,
            Some("allow" | "prefer") | None => config.tls.mode = TlsMode::Prefer,
            Some("require") => {
                config.tls.mode = TlsMode::Require;
                config.tls.accept_invalid = true;
            }
            Some(_) => config.tls.mode = TlsMode::Require,
        }
        config.tls.ca_file = text(params, "sslrootcert")
            .or_else(|| text(s, "SSLRootCert"))
            .map(PathBuf::from);
        config.tls.client_cert = text(params, "sslcert")
            .or_else(|| text(s, "SSLCert"))
            .map(PathBuf::from);
        config.tls.client_key = text(params, "sslkey")
            .or_else(|| text(s, "SSLKey"))
            .map(PathBuf::from);
        if let Some(bg) = text(s, "BGColor").filter(|c| c.len() == 7 && c.starts_with('#')) {
            config.color = Some(bg.to_ascii_lowercase());
        }
        if num(s, "UseSSHTunnel") == Some(1)
            && let Some(th) = text(s, "TunnelHost")
        {
            let auth = if num(s, "TunnelAuthentication") == Some(1) {
                match text(s, "TunnelIdentityFile") {
                    Some(p) => SshAuth::KeyFile {
                        path: PathBuf::from(p),
                    },
                    None => SshAuth::Agent,
                }
            } else {
                SshAuth::Password
            };
            config.ssh = Some(SshConfig {
                target: SshHop {
                    host: th,
                    port: num(s, "TunnelPort")
                        .and_then(|p| u16::try_from(p).ok())
                        .unwrap_or(22),
                    user: text(s, "TunnelUsername").unwrap_or_else(|| "root".into()),
                    auth,
                },
                jump: None,
            });
        }
        if text(s, "Service").is_some() {
            notes.push("pg_service.conf services are not supported; the host and port were imported instead.".into());
        }
        report.imported.push(Imported {
            config,
            secrets: Vec::new(),
            notes,
        });
    }
    Ok(report)
}
