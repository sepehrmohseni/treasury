//! What the app's commands take and return. The app (native) and the interface (WebAssembly)
//! both use these types, so a command and its caller always agree.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use treasury_core::analyze::StatementClass;
use treasury_core::config::{ConnectionConfig, Engine, SecretSlot};
use treasury_core::driver::{Capabilities, ServerInfo};
use treasury_core::format::Cell;

/// Facts the interface needs at start.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppFacts {
    /// The app's version.
    pub version: String,
    /// No system keychain was found; passwords last until Treasury quits.
    pub secrets_in_memory: bool,
}

/// A saved connection and which secrets it has (never their values).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectionView {
    /// Its settings.
    pub config: ConnectionConfig,
    /// The secrets saved for it.
    pub secrets: Vec<SecretSlot>,
}

/// A secret typed in the connection form. An empty value keeps the saved one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecretInput {
    /// Which secret.
    pub slot: SecretSlot,
    /// Its value.
    pub value: String,
}

/// An open connection, as the status bar shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveView {
    /// Product, version and other facts.
    pub info: ServerInfo,
    /// What the session supports.
    pub capabilities: Capabilities,
    /// Writes are refused right now.
    pub read_only: bool,
    /// Seconds until "Unlock writes" ends.
    pub unlocked_secs: Option<u64>,
}

/// What must happen before a statement runs, from the guardrails.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    /// Pass this when running the statement.
    pub token: u64,
    /// What the statement is.
    pub class: StatementClass,
    /// The leading keywords, for the dialog.
    pub summary: String,
    /// Why it cannot run, when it cannot.
    pub refused: Option<String>,
    /// The rows it changes, when they were counted.
    pub count: Option<u64>,
    /// Why the connection name must be typed, when it must.
    pub name_required: Option<String>,
}

/// Rows of a result, formatted for display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowView {
    /// Index of the first row.
    pub start: u64,
    /// The rows.
    pub rows: Vec<Vec<Cell>>,
}

/// The export formats offered for a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// CSV with a header row.
    Csv,
    /// A JSON array of objects.
    Json,
    /// One JSON object per line.
    Ndjson,
    /// Apache Parquet.
    Parquet,
}

impl ExportFormat {
    /// Every format, in menu order.
    pub const ALL: [ExportFormat; 4] = [Self::Csv, Self::Json, Self::Ndjson, Self::Parquet];

    /// The file extension.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Ndjson => "ndjson",
            Self::Parquet => "parquet",
        }
    }

    /// The menu label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::Ndjson => "NDJSON",
            Self::Parquet => "Parquet",
        }
    }
}

/// What an export wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportView {
    /// Rows written.
    pub rows: u64,
    /// File size.
    pub bytes: u64,
    /// Where it is.
    pub path: PathBuf,
}

/// One connection an import would bring over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportItem {
    /// Its position in the file, for choosing it.
    pub index: usize,
    /// The name it had.
    pub name: String,
    /// Its engine.
    pub engine: Engine,
    /// Host and port, or the endpoint as text.
    pub endpoint: String,
    /// Database.
    pub database: Option<String>,
    /// User.
    pub user: Option<String>,
    /// How many secrets come with it; their values never leave Rust's native side.
    pub secrets: usize,
    /// What the import could not carry over.
    pub notes: Vec<String>,
}

/// What an import file holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPreview {
    /// The client the file came from.
    pub source: String,
    /// Connections that can be imported.
    pub items: Vec<ImportItem>,
    /// Connections that cannot, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// An SSH host key seen for the first time, for the user to accept or refuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKeyQuestion {
    /// Pass this back with the answer.
    pub id: u64,
    /// The SSH host.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// The key algorithm.
    pub algorithm: String,
    /// The SHA-256 fingerprint as OpenSSH prints it.
    pub fingerprint: String,
}
