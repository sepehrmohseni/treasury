//! Exporting and importing the connection list, which is also how connections move from the
//! desktop to a phone. Without a passphrase the file holds no secrets. With one, the whole file
//! (secrets included) is sealed with Argon2id and XChaCha20-Poly1305.

use crate::{Result, StoreError};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use treasury_core::config::{ConnectionConfig, ConnectionId, SecretSlot};
use treasury_vault::{Envelope, SecretStore};
use zeroize::Zeroizing;

const FORMAT: &str = "treasury-connections";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Document {
    format: String,
    version: u32,
    connections: Vec<ConnectionConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    secrets: BTreeMap<String, BTreeMap<SecretSlot, String>>,
}

/// A connection read from a file, with the secrets the file carried. It gets a new identifier,
/// so importing the same file twice never overwrites anything.
pub struct Imported {
    /// The settings.
    pub config: ConnectionConfig,
    /// Secrets, to put in the SecretStore under the new identifier.
    pub secrets: Vec<(SecretSlot, SecretString)>,
}

/// Writes `connections` to an export file. With `passphrase`, their secrets are included and
/// the file is sealed.
pub fn export(
    connections: &[ConnectionConfig],
    store: &dyn SecretStore,
    passphrase: Option<&SecretString>,
) -> Result<Vec<u8>> {
    let mut doc = Document {
        format: FORMAT.into(),
        version: VERSION,
        connections: connections.to_vec(),
        secrets: BTreeMap::new(),
    };
    let Some(passphrase) = passphrase else {
        return serde_json::to_vec_pretty(&doc).map_err(|_| StoreError::Decode("export"));
    };
    for c in connections {
        let mut slots = BTreeMap::new();
        for slot in c.secret_slots() {
            if let Some(v) = store.get(&slot.key(&c.id))? {
                slots.insert(slot, v.expose_secret().to_owned());
            }
        }
        if !slots.is_empty() {
            doc.secrets.insert(c.id.0.clone(), slots);
        }
    }
    let json = Zeroizing::new(serde_json::to_vec(&doc).map_err(|_| StoreError::Decode("export"))?);
    for slots in doc.secrets.values_mut() {
        for v in slots.values_mut() {
            zeroize::Zeroize::zeroize(v);
        }
    }
    Ok(Envelope::seal(passphrase, &json)?)
}

/// Whether an export file is sealed and needs a passphrase.
pub fn needs_passphrase(bytes: &[u8]) -> bool {
    Envelope::looks_like(bytes)
}

/// Reads an export file.
pub fn import(bytes: &[u8], passphrase: Option<&SecretString>) -> Result<Vec<Imported>> {
    let plain: Zeroizing<Vec<u8>> = if Envelope::looks_like(bytes) {
        let p = passphrase.ok_or(StoreError::NeedsPassphrase)?;
        Envelope::open(p, bytes)?
    } else {
        Zeroizing::new(bytes.to_vec())
    };
    let mut doc: Document =
        serde_json::from_slice(&plain).map_err(|_| StoreError::Decode("connection export"))?;
    if doc.format != FORMAT {
        return Err(StoreError::Decode("connection export"));
    }
    if doc.version > VERSION {
        return Err(StoreError::Decode("connection export (newer format)"));
    }
    let mut out = Vec::with_capacity(doc.connections.len());
    for mut config in doc.connections {
        let old = config.id.0.clone();
        config.id = ConnectionId::random()?;
        config.validate()?;
        let secrets = doc
            .secrets
            .remove(&old)
            .unwrap_or_default()
            .into_iter()
            .map(|(slot, v)| (slot, SecretString::from(v)))
            .collect();
        out.push(Imported { config, secrets });
    }
    Ok(out)
}
