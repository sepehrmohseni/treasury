//! Opening the database file privately and bringing its schema up to date.

use crate::{Result, StoreError};
use rusqlite::Connection;
use std::path::Path;

/// Schema versions, applied in order; `PRAGMA user_version` records the last one applied.
const MIGRATIONS: &[&str] = &[
    // 1: the initial schema.
    "CREATE TABLE connections (
        id TEXT PRIMARY KEY,
        position INTEGER NOT NULL,
        config TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE saved_queries (
        id TEXT PRIMARY KEY,
        folder TEXT NOT NULL DEFAULT '',
        name TEXT NOT NULL,
        connection_id TEXT,
        text BLOB NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        connection_id TEXT NOT NULL,
        text BLOB NOT NULL,
        executed_at INTEGER NOT NULL,
        duration_ms INTEGER NOT NULL,
        rows INTEGER,
        status TEXT NOT NULL
    );
    CREATE INDEX history_by_connection ON history (connection_id, id);
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE tabs (
        id TEXT PRIMARY KEY,
        position INTEGER NOT NULL,
        connection_id TEXT,
        title TEXT NOT NULL,
        text BLOB NOT NULL,
        cursor INTEGER NOT NULL DEFAULT 0,
        saved_query TEXT
    );
    CREATE TABLE snapshots (
        connection_id TEXT NOT NULL,
        taken_at INTEGER NOT NULL,
        metrics BLOB NOT NULL
    );
    CREATE INDEX snapshots_by_connection ON snapshots (connection_id, taken_at);",
];

/// Opens `path`, creating it readable and writable by the owner only. SQLite gives its WAL and
/// shared-memory files the same permissions.
pub fn open_file(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
    }
    if !path.exists() {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(StoreError::Io(e.to_string())),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| StoreError::Io(e.to_string()))?;
    }
    Ok(Connection::open(path)?)
}

/// Sets connection pragmas, applies pending migrations and creates the in-memory search index.
pub fn prepare(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;",
    )?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if version > known {
        return Err(StoreError::Decode("database schema"));
    }
    for (i, sql) in MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(version).unwrap_or(0))
    {
        let next = i64::try_from(i + 1).unwrap_or(i64::MAX);
        conn.execute_batch(&format!(
            "BEGIN; {sql}; PRAGMA user_version = {next}; COMMIT;"
        ))?;
    }
    // The search index lives in the temp schema, which `temp_store = MEMORY` keeps off disk.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.history_fts USING fts5(text, tokenize = 'unicode61');",
    )?;
    Ok(())
}
