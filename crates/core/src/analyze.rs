//! What the core needs to know about a statement before it runs: its class, how to cap its
//! rows, how to count the rows it would change. Each query language implements [`Analyzer`]:
//! the SQL dialects in `treasury-sql`, MongoDB, Redis and CQL in their drivers.

use serde::{Deserialize, Serialize};
use std::ops::Range;

/// What a statement does, from least to most dangerous. `Unknown` always counts as a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementClass {
    /// Reads data or metadata and changes nothing.
    Read,
    /// Changes rows, documents or keys.
    Write,
    /// Changes schema: CREATE, ALTER, DROP, TRUNCATE.
    Ddl,
    /// Changes server state, users or privileges: GRANT, KILL, CONFIG SET, VACUUM.
    Admin,
    /// Treasury cannot tell. Treated as a write everywhere.
    Unknown,
}

impl StatementClass {
    /// True for every class that may change something, which includes `Unknown`.
    pub fn may_write(self) -> bool {
        self != StatementClass::Read
    }
}

/// A destructive schema statement that production connections confirm by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destructive {
    /// DROP.
    Drop,
    /// TRUNCATE.
    Truncate,
    /// ALTER.
    Alter,
}

/// Transaction control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxControl {
    /// BEGIN / START TRANSACTION. `read_write` is `Some(true)` for an explicit READ WRITE.
    Begin {
        /// The access mode written in the statement, if any.
        read_write: Option<bool>,
    },
    /// COMMIT.
    Commit,
    /// ROLLBACK (not to a savepoint).
    Rollback,
    /// SAVEPOINT, RELEASE, ROLLBACK TO.
    Savepoint,
}

/// Facts about a statement beyond its class.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flags {
    /// Takes row or table locks (SELECT ... FOR UPDATE, LOCK TABLE).
    pub locks: bool,
    /// Transaction control.
    pub transaction: Option<TxControl>,
    /// Would turn off server-enforced read-only (`SET default_transaction_read_only = off`,
    /// `RESET ALL`, `SET SESSION TRANSACTION READ WRITE`, `BEGIN READ WRITE`).
    pub read_only_escape: bool,
    /// DROP, TRUNCATE or ALTER.
    pub destructive: Option<Destructive>,
    /// An UPDATE or DELETE without a WHERE clause.
    pub unfiltered: bool,
    /// EXPLAIN ANALYZE and similar: running it executes the inner statement.
    pub executes: bool,
    /// Never allowed on production, even with writes unlocked (Redis FLUSHALL, SHUTDOWN, ...).
    pub production_forbidden: bool,
}

/// One statement of a script and what Treasury found out about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classified {
    /// Byte range of the statement in the script, without the terminator.
    pub range: Range<usize>,
    /// What it does.
    pub class: StatementClass,
    /// Further facts.
    pub flags: Flags,
    /// The leading keywords, for confirmation dialogs: `DELETE FROM orders`.
    pub summary: String,
    /// Why the class is `Unknown`, when it is.
    pub reason: Option<String>,
}

/// How to cap a statement at the default row limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitPlan {
    /// A simple SELECT without a limit: run `text`, which adds the dialect's limit clause.
    /// Server error positions at or after `inserted_at` are `inserted_len` too far right.
    Rewritten {
        /// The statement with the limit.
        text: String,
        /// Byte offset where the limit clause was inserted.
        inserted_at: usize,
        /// Length of the inserted text, in bytes.
        inserted_len: usize,
    },
    /// The statement already limits itself, or cannot be rewritten safely: read at most the
    /// limit and leave the rest on the server.
    StopFetching,
    /// Not a statement that returns rows.
    NotApplicable,
}

/// How to count the rows an UPDATE or DELETE would change, without running it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CountPlan {
    /// Run `query` on the same session; it returns one row with one integer column.
    Query(String),
    /// A form the rewriter cannot handle; the user confirms by typing the connection name.
    Underivable {
        /// What made it underivable, for the dialog.
        reason: String,
    },
    /// Not an UPDATE or DELETE.
    NotApplicable,
}

/// A placeholder in a statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placeholder {
    /// `$1`, `?`, `:name`, `@name` as written.
    pub text: String,
    /// Byte range in the statement.
    pub range: Range<usize>,
}

/// A query language's analysis rules.
pub trait Analyzer: Send + Sync {
    /// Splits `script` into statements and classifies each one. Text that cannot be parsed is
    /// returned as one `Unknown` statement rather than guessed at.
    fn classify(&self, script: &str) -> Vec<Classified>;

    /// How to cap `statement` (a single statement) at `limit` rows.
    fn limit(&self, statement: &str, limit: u64) -> LimitPlan;

    /// How to preview the rows `statement` would change.
    fn count(&self, statement: &str) -> CountPlan;

    /// The bind placeholders in `statement`, in order.
    fn placeholders(&self, statement: &str) -> Vec<Placeholder>;
}

/// The most dangerous class in a script; an empty script reads nothing.
pub fn script_class(statements: &[Classified]) -> StatementClass {
    statements
        .iter()
        .map(|s| s.class)
        .max()
        .unwrap_or(StatementClass::Read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(class: StatementClass) -> Classified {
        Classified {
            range: 0..0,
            class,
            flags: Flags::default(),
            summary: String::new(),
            reason: None,
        }
    }

    #[test]
    fn unknown_outranks_everything() {
        use StatementClass::*;
        assert_eq!(script_class(&[st(Read), st(Unknown), st(Write)]), Unknown);
        assert_eq!(script_class(&[st(Read), st(Ddl)]), Ddl);
        assert_eq!(script_class(&[]), Read);
        assert!(Unknown.may_write());
        assert!(!Read.may_write());
    }
}
