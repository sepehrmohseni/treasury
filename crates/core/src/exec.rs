//! What a driver produces when it runs a statement: a stream of events, so result sets of any
//! size arrive page by page and the caller decides how far to read.

use crate::error::{Error, Result};
use crate::value::Value;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The logical type of a column, used for alignment, sorting, editing and export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    /// Booleans.
    Bool,
    /// Integers of any width.
    Integer,
    /// Binary floating point.
    Float,
    /// Exact decimals.
    Decimal,
    /// Text.
    Text,
    /// Binary data.
    Binary,
    /// Dates.
    Date,
    /// Times of day, with or without zone.
    Time,
    /// Timestamps without zone.
    Timestamp,
    /// Timestamps with zone.
    TimestampTz,
    /// Intervals and durations.
    Interval,
    /// UUIDs.
    Uuid,
    /// JSON.
    Json,
    /// Arrays, lists, sets and tuples.
    List,
    /// Composite types and UDTs.
    Composite,
    /// Maps.
    Map,
    /// Documents (MongoDB).
    Document,
    /// BSON ObjectIds.
    ObjectId,
    /// Mixed or unknown (MongoDB fields, untyped Redis values).
    Any,
    /// Anything else.
    Other,
}

impl ValueKind {
    /// Whether values of this kind are right-aligned in the grid.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            ValueKind::Integer | ValueKind::Float | ValueKind::Decimal
        )
    }
}

/// Where a result column comes from, when the server says, which is what makes a result
/// editable.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnSource {
    /// Schema or database.
    pub schema: Option<String>,
    /// Table.
    pub table: String,
    /// Column name in the table.
    pub column: String,
}

/// A result column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// The column label.
    pub name: String,
    /// The engine's type name, as it would appear in DDL.
    pub type_name: String,
    /// The logical kind.
    pub kind: ValueKind,
    /// Whether NULL is allowed, when known.
    pub nullable: Option<bool>,
    /// The table column it was read from, when known.
    pub source: Option<ColumnSource>,
}

impl Column {
    /// A column with a name, type and kind and nothing else known.
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, kind: ValueKind) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            kind,
            nullable: None,
            source: None,
        }
    }
}

/// One row: values in column order.
pub type Row = Vec<Value>;

/// A bind parameter value, positional or named.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Param {
    /// The placeholder's name (`:id`, `@id`) or `None` for positional placeholders.
    pub name: Option<String>,
    /// The value to bind.
    pub value: Value,
}

/// A statement to run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// The statement text, exactly as it should reach the server.
    pub text: String,
    /// Bind parameters, sent separately from the text.
    pub params: Vec<Param>,
    /// Stop reading rows after this many, leaving the rest on the server.
    pub max_rows: Option<u64>,
    /// How many rows to ask the server for at a time.
    pub fetch_rows: u32,
    /// Server-side time limit, used for the metadata queries Treasury issues by itself.
    pub timeout: Option<Duration>,
}

impl ExecRequest {
    /// A request for `text` with no parameters and default fetch size.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            params: Vec::new(),
            max_rows: None,
            fetch_rows: 500,
            timeout: None,
        }
    }

    /// Sets a server-side time limit.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Stops after `n` rows.
    pub fn with_max_rows(mut self, n: u64) -> Self {
        self.max_rows = Some(n);
        self
    }
}

/// How serious a server message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Informational output (PRINT, RAISE NOTICE).
    Info,
    /// A warning the user should see.
    Warning,
}

/// A message the server sent alongside results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// Severity.
    pub level: NoticeLevel,
    /// The text.
    pub message: String,
    /// The engine's code, if any.
    pub code: Option<String>,
}

/// One step of a running statement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExecEvent {
    /// A result set begins with these columns.
    ResultStart(Vec<Column>),
    /// Rows of the current result set.
    Rows(Vec<Row>),
    /// The current result set (or a statement without one) is complete.
    ResultEnd {
        /// Rows inserted, updated or deleted, when the statement changed data.
        affected: Option<u64>,
        /// Reading stopped at `max_rows`; more rows exist on the server.
        truncated: bool,
    },
    /// A server message.
    Notice(Notice),
}

/// The events of one execution. Rows are produced only as the stream is polled, so a slow
/// reader keeps the server-side cursor where it is instead of buffering the whole result.
pub type ExecStream = BoxStream<'static, Result<ExecEvent>>;

/// One result set's state, as the UI sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetStatus {
    /// Columns; empty for a statement that returns no rows.
    pub columns: Vec<Column>,
    /// Rows fetched so far and kept.
    pub rows: u64,
    /// The server has no more rows for this set.
    pub complete: bool,
    /// Reading stopped at the row limit; more rows exist.
    pub limited: bool,
    /// Rows changed, for statements that change data.
    pub affected: Option<u64>,
    /// An export read the rest of this set straight to a file; rows after `rows` are not kept.
    pub exported_rest: bool,
}

/// The execution as a whole.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionStatus {
    /// Identifier.
    pub id: u64,
    /// Result sets so far.
    pub sets: Vec<SetStatus>,
    /// Server messages.
    pub notices: Vec<Notice>,
    /// Notices beyond the stored maximum.
    pub dropped_notices: u64,
    /// The stream has ended (finished, failed, cancelled or stopped at the limit).
    pub finished: bool,
    /// The error that ended it, if one did.
    pub error: Option<Error>,
}
