//! What the app's commands take and return. The app (native) and the interface (WebAssembly)
//! both use these types, so a command and its caller always agree.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use treasury_core::analyze::StatementClass;
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine, SecretSlot};
use treasury_core::driver::{Capabilities, ServerInfo};
use treasury_core::format::Cell;
use treasury_core::schema::{ObjectKind, ObjectPath};

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

/// A new value for one cell of a result, as the grid collects it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellEdit {
    /// The row's index in the result.
    pub row: u64,
    /// The column's index in the result.
    pub column: usize,
    /// The text typed, or `None` for NULL.
    pub value: Option<String>,
}

/// The edits pending on one result set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowEdits {
    /// Changed cells, oldest first.
    pub cells: Vec<CellEdit>,
    /// Rows to delete, by index.
    pub deletes: Vec<u64>,
}

/// Whether a result set's rows can be edited.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditInfo {
    /// The table the rows belong to, when they can be edited.
    pub table: Option<String>,
    /// Which result columns can be edited, in order.
    pub columns: Vec<bool>,
    /// Why the rows cannot be edited, when they cannot.
    pub reason: Option<String>,
}

/// A newer release than the one running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateView {
    /// Its version.
    pub version: String,
    /// What changed, as the release notes say.
    pub notes: Option<String>,
}

/// An editor tab. Its text is kept as it is typed, so it survives restarts and crashes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabView {
    /// Identifier.
    pub id: String,
    /// The connection whose statements it holds.
    pub connection: ConnectionId,
    /// What the tab strip shows.
    pub title: String,
    /// The editor text, saved as a query or not.
    pub text: String,
    /// Where the cursor was, in characters.
    pub cursor: u32,
    /// The saved query the tab shows, if any.
    pub saved_query: Option<String>,
}

/// A saved query, without its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQueryView {
    /// Identifier.
    pub id: String,
    /// Folder, as `/`-separated names; empty for none.
    pub folder: String,
    /// Name.
    pub name: String,
    /// The connection it was saved from; `None` for queries shown on every connection.
    pub connection: Option<ConnectionId>,
}

/// How a statement in the history ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// It finished.
    Ok,
    /// It failed with an error.
    Failed,
    /// It was cancelled.
    Cancelled,
    /// It had not ended when the entry was read: still running, or Treasury quit first.
    Unfinished,
}

/// A statement in the history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryView {
    /// Identifier.
    pub id: i64,
    /// The statement as it ran.
    pub text: String,
    /// When it started, Unix milliseconds.
    pub executed_at: i64,
    /// How long it ran.
    pub duration_ms: i64,
    /// Rows returned or changed, when known.
    pub rows: Option<i64>,
    /// How it ended.
    pub outcome: RunOutcome,
}

/// The longest name or folder a saved query may have, in characters.
pub const NAME_LIMIT: usize = 200;

/// A folder as the user typed it, made canonical: names trimmed, empty names dropped, joined
/// with `/`. `" reports / / monthly "` becomes `"reports/monthly"`.
pub fn folder_path(typed: &str) -> String {
    typed
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// The title of a new tab: "Query" and the lowest number no tab in `titles` has yet.
pub fn new_tab_title<'a>(titles: impl IntoIterator<Item = &'a str>) -> String {
    let used: Vec<u32> = titles
        .into_iter()
        .filter_map(|t| t.strip_prefix("Query ")?.parse().ok())
        .collect();
    let n = (1..).find(|n| !used.contains(n)).unwrap_or(1);
    format!("Query {n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_canonical() {
        assert_eq!(folder_path(" reports / / monthly "), "reports/monthly");
        assert_eq!(folder_path("  "), "");
        assert_eq!(folder_path("/a/"), "a");
    }

    #[test]
    fn new_tabs_take_the_lowest_free_number() {
        assert_eq!(new_tab_title([]), "Query 1");
        assert_eq!(new_tab_title(["Query 1", "orders", "Query 3"]), "Query 2");
        assert_eq!(new_tab_title(["Query 2", "Query x"]), "Query 1");
    }
}

/// A table, view or collection of a connection, for completion and the command palette.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Where it is in the object tree.
    pub path: ObjectPath,
    /// What it is.
    pub kind: ObjectKind,
    /// Its name.
    pub name: String,
    /// Its schema (or keyspace), if the engine has them.
    pub schema: Option<String>,
    /// Its database (or catalog), if the engine has them.
    pub database: Option<String>,
}

/// The tables, views and collections of a connection, as far as they were read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogView {
    /// What was found.
    pub entries: Vec<CatalogEntry>,
    /// False when there were more than could be read, or part of the tree could not be read.
    pub complete: bool,
}

/// A column, for completion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnView {
    /// Its name.
    pub name: String,
    /// Its type, as in DDL.
    pub type_name: String,
}
