//! MongoDB Compass connection exports (Export Connections). Exports without a passphrase carry
//! no passwords; encrypted exports are imported without their secrets.

use crate::{Draft, Imported, Report, unreadable};
use serde_json::Value as J;
use std::path::Path;
use treasury_core::Result;
use treasury_core::config::{Engine, SecretSlot};

pub fn import(path: &Path) -> Result<Report> {
    let bytes = std::fs::read(path).map_err(|e| unreadable("MongoDB Compass", e))?;
    let doc: J = serde_json::from_slice(&bytes).map_err(|e| unreadable("MongoDB Compass", e))?;
    let list = doc
        .get("connections")
        .and_then(J::as_array)
        .ok_or_else(|| unreadable("MongoDB Compass", "there is no \"connections\" list"))?;
    let mut report = Report::default();
    for c in list {
        let name = c
            .pointer("/favorite/name")
            .and_then(J::as_str)
            .unwrap_or("Compass connection");
        let Some(uri) = c
            .pointer("/connectionOptions/connectionString")
            .and_then(J::as_str)
        else {
            continue;
        };
        let mut config = Draft::new(name, Engine::MongoDb).build()?;
        let mut secrets = Vec::new();
        if let Some(pw) = crate::apply_uri(&mut config, uri) {
            secrets.push((SecretSlot::Password, pw));
        }
        let mut notes = Vec::new();
        if c.get("connectionSecrets").is_some() {
            notes.push(
                "The export was encrypted with a passphrase; its passwords were not imported."
                    .into(),
            );
        } else if secrets.is_empty() {
            notes.push(
                "Compass exports leave passwords out unless encrypted; enter it before connecting."
                    .into(),
            );
        }
        if c.pointer("/connectionOptions/sshTunnel").is_some() {
            notes
                .push("The SSH tunnel was not imported; add it in the connection settings.".into());
        }
        report.imported.push(Imported {
            config,
            secrets,
            notes,
        });
    }
    Ok(report)
}
