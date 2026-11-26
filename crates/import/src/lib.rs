//! Importing saved connections from other database clients, so people can move to Treasury
//! without retyping them.
//!
//! Supported: DBeaver (a project export `.dbp`, a zip holding one, a workspace or project
//! folder, or `data-sources.json` itself, with the passwords DBeaver saved), Beekeeper Studio
//! (its data folder, with saved passwords), pgAdmin 4 (`servers.json` exports), JetBrains
//! DataGrip and IDE data sources (`dataSources.xml`) and MongoDB Compass connection exports.
//! Passwords that the other client keeps in the system keychain are not readable and are left
//! for the user to enter.
//!
//! Every connection gets a new Treasury identifier. Connections to engines Treasury does not
//! support are listed as skipped with the reason, never dropped silently.

#![forbid(unsafe_code)]

mod beekeeper;
mod compass;
mod crypto;
mod dbeaver;
mod jetbrains;
mod pgadmin;

use secrecy::SecretString;
use std::path::Path;
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, Environment, SecretSlot, TlsMode};
use treasury_core::{Error, Result};

/// The clients Treasury imports from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// DBeaver.
    DBeaver,
    /// Beekeeper Studio.
    Beekeeper,
    /// pgAdmin 4.
    PgAdmin,
    /// JetBrains DataGrip and IDE database tools.
    JetBrains,
    /// MongoDB Compass.
    Compass,
}

impl Source {
    /// The client's name, as the import dialog shows it.
    pub fn display_name(self) -> &'static str {
        match self {
            Source::DBeaver => "DBeaver",
            Source::Beekeeper => "Beekeeper Studio",
            Source::PgAdmin => "pgAdmin",
            Source::JetBrains => "DataGrip and JetBrains IDEs",
            Source::Compass => "MongoDB Compass",
        }
    }
}

/// One imported connection.
pub struct Imported {
    /// The new connection's settings.
    pub config: ConnectionConfig,
    /// Secrets to store under the new connection.
    pub secrets: Vec<(SecretSlot, SecretString)>,
    /// What could not be carried over, for the import summary.
    pub notes: Vec<String>,
}

/// A connection that was not imported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// Its name in the other client.
    pub name: String,
    /// Why.
    pub reason: String,
}

/// The result of an import.
#[derive(Default)]
pub struct Report {
    /// Connections to save.
    pub imported: Vec<Imported>,
    /// Connections left out.
    pub skipped: Vec<Skipped>,
}

/// Guesses the source of `path` from its name and contents.
pub fn detect(path: &Path) -> Option<Source> {
    if path.is_dir() {
        if path.join("app.db").exists() && path.join(".key").exists() {
            return Some(Source::Beekeeper);
        }
        if dbeaver::find_in_dir(path).is_some() {
            return Some(Source::DBeaver);
        }
        if path.join("dataSources.xml").exists()
            || path.join(".idea").join("dataSources.xml").exists()
        {
            return Some(Source::JetBrains);
        }
        return None;
    }
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".dbp") || name.ends_with(".zip") || name.starts_with("data-sources") {
        return Some(Source::DBeaver);
    }
    if name == "datasources.xml" || name == "datasources.local.xml" {
        return Some(Source::JetBrains);
    }
    if name.ends_with(".json") {
        let text = std::fs::read_to_string(path).ok()?;
        if text.contains("\"Servers\"") {
            return Some(Source::PgAdmin);
        }
        if text.contains("\"connectionOptions\"") {
            return Some(Source::Compass);
        }
        if text.contains("\"connections\"") && text.contains("\"provider\"") {
            return Some(Source::DBeaver);
        }
    }
    None
}

/// Imports the connections in `path` from `source`.
pub fn import(path: &Path, source: Source) -> Result<Report> {
    match source {
        Source::DBeaver => dbeaver::import(path),
        Source::Beekeeper => beekeeper::import(path),
        Source::PgAdmin => pgadmin::import(path),
        Source::JetBrains => jetbrains::import(path),
        Source::Compass => compass::import(path),
    }
}

pub(crate) fn unreadable(source: &str, detail: impl std::fmt::Display) -> Error {
    Error::config(
        None,
        format!("This is not a {source} export Treasury can read: {detail}."),
    )
}

/// Builds a connection with the settings every importer shares.
pub(crate) struct Draft {
    pub name: String,
    pub engine: Engine,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub environment: Environment,
    pub folder: Option<String>,
    pub read_only: bool,
}

impl Draft {
    pub fn new(name: &str, engine: Engine) -> Self {
        Self {
            name: name.to_owned(),
            engine,
            host: None,
            port: None,
            database: None,
            user: None,
            environment: Environment::Development,
            folder: None,
            read_only: false,
        }
    }

    pub fn build(self) -> Result<ConnectionConfig> {
        let name = if self.name.trim().is_empty() {
            format!("{} connection", self.engine.display_name())
        } else {
            self.name
        };
        let host = self
            .host
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "localhost".into());
        let mut c = ConnectionConfig::new(&name, self.engine, &host)?;
        c.endpoint = Endpoint::Host {
            host,
            port: self
                .port
                .filter(|p| *p > 0)
                .unwrap_or(self.engine.default_port()),
        };
        c.database = self.database.filter(|d| !d.is_empty());
        c.user = self.user.filter(|u| !u.is_empty());
        c.environment = self.environment;
        c.folder = self.folder.filter(|f| !f.is_empty());
        c.read_only = self.read_only;
        if c.environment == Environment::Local {
            c.tls.mode = TlsMode::Prefer;
        }
        Ok(c)
    }
}

/// Applies a pasted-style connection string to a draft-built config.
pub(crate) fn apply_uri(c: &mut ConnectionConfig, uri: &str) -> Option<SecretString> {
    let parsed = treasury_core::uri::parse(uri).ok()?;
    if let Some(e) = parsed.endpoint {
        c.endpoint = e;
    }
    if parsed.database.is_some() {
        c.database = parsed.database;
    }
    if parsed.user.is_some() {
        c.user = parsed.user;
    }
    if let Some(t) = parsed.tls {
        c.tls.mode = t;
    }
    c.tls.accept_invalid |= parsed.accept_invalid_certs;
    c.options.extend(parsed.options);
    parsed.password
}

/// A local host, which makes the connection's environment Local.
pub(crate) fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0") || host.ends_with(".localhost")
}

#[cfg(test)]
mod tests;
