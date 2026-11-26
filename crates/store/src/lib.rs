//! The local app database: saved connections, saved queries, query history, settings, open tabs
//! and overview snapshots, in one SQLite file readable by the user only.
//!
//! Query text (history, saved queries, unsaved editor buffers) is sealed field by field with a
//! key kept in the SecretStore, so the file alone does not reveal what was run. History search
//! uses SQLite FTS5 over an index that lives only in memory: it is built from the decrypted rows
//! the first time the user searches and is never written to disk.

#![forbid(unsafe_code)]

mod db;
pub mod transfer;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use treasury_core::config::{ConnectionConfig, ConnectionId};
use treasury_core::health::Metric;
use treasury_vault::{Sealer, SecretStore};
use zeroize::Zeroizing;

/// Store errors. None of them contains query text or secrets.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("The local database reported an error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored value could not be decoded.
    #[error("A stored {0} could not be read; it may be from a newer version of Treasury.")]
    Decode(&'static str),
    /// Sealed text could not be opened: the field key changed or the data was altered.
    #[error(
        "Stored query text could not be decrypted. The key in the system keychain may have been \
         removed; purging history removes the unreadable entries."
    )]
    Sealed,
    /// The file could not be created with private permissions.
    #[error("The local database could not be created: {0}")]
    Io(String),
    /// Secret storage failed.
    #[error(transparent)]
    Vault(#[from] treasury_vault::VaultError),
    /// The file is sealed and no passphrase was given.
    #[error("This file is encrypted. Enter the passphrase it was exported with.")]
    NeedsPassphrase,
    /// A connection in the file is invalid.
    #[error(transparent)]
    Core(#[from] treasury_core::Error),
}

impl From<StoreError> for treasury_core::Error {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Core(inner) => inner,
            StoreError::Vault(v) => v.into(),
            StoreError::NeedsPassphrase => {
                treasury_core::Error::new(treasury_core::ErrorKind::Auth, e.to_string())
            }
            other => treasury_core::Error::internal(other.to_string()),
        }
    }
}

/// Result of store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// The SecretStore key holding the field encryption key.
pub const FIELD_KEY: &str = "treasury/store-field-key";

/// The field encryption key from `secrets`, created on first use.
pub fn field_key(secrets: &dyn SecretStore) -> Result<Zeroizing<[u8; 32]>> {
    use secrecy::ExposeSecret;
    if let Some(hex) = secrets.get(FIELD_KEY)? {
        return decode_key(hex.expose_secret()).ok_or(StoreError::Decode("field key"));
    }
    let key = treasury_vault::random_key()?;
    let hex = Zeroizing::new(treasury_core::format::hex(key.as_slice()));
    secrets.set(
        FIELD_KEY,
        &secrecy::SecretString::from(hex.as_str().to_owned()),
    )?;
    Ok(key)
}

fn decode_key(hex: &str) -> Option<Zeroizing<[u8; 32]>> {
    if hex.len() != 64 {
        return None;
    }
    let mut key = Zeroizing::new([0u8; 32]);
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

/// A saved query, without its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQuery {
    /// Identifier.
    pub id: String,
    /// Folder, `/`-separated; empty for the top level.
    pub folder: String,
    /// Name.
    pub name: String,
    /// The connection it belongs to, if any.
    pub connection: Option<ConnectionId>,
    /// Last change, Unix milliseconds.
    pub updated_at: i64,
}

/// How a history entry ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Finished.
    Ok,
    /// Failed with an error.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "ok" => RunStatus::Ok,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Failed,
        }
    }
}

/// One history entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Row id (0 for an entry not yet stored).
    pub id: i64,
    /// The connection it ran on.
    pub connection: ConnectionId,
    /// The statement text.
    pub text: String,
    /// When it started, Unix milliseconds.
    pub executed_at: i64,
    /// How long it ran.
    pub duration_ms: i64,
    /// Rows returned or affected, when known.
    pub rows: Option<i64>,
    /// How it ended.
    pub status: RunStatus,
}

/// An open editor tab, restored after a restart or crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabState {
    /// Identifier.
    pub id: String,
    /// Order in the tab strip.
    pub position: i64,
    /// The tab's connection.
    pub connection: Option<ConnectionId>,
    /// Title.
    pub title: String,
    /// Editor text, saved or not.
    pub text: String,
    /// Cursor offset.
    pub cursor: i64,
    /// The saved query the tab was opened from.
    pub saved_query: Option<String>,
}

/// The local database.
pub struct Store {
    conn: Mutex<rusqlite::Connection>,
    sealer: Sealer,
    /// Whether the in-memory FTS index has been built in this process.
    fts_ready: Mutex<bool>,
}

const HISTORY_CTX: &[u8] = b"history.text";
const SAVED_CTX: &[u8] = b"saved_queries.text";
const TAB_CTX: &[u8] = b"tabs.text";

impl Store {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: &Path, sealer: Sealer) -> Result<Self> {
        let conn = db::open_file(path)?;
        Self::with_connection(conn, sealer)
    }

    /// A database in memory, for tests and for sessions that must leave nothing behind.
    pub fn open_in_memory(sealer: Sealer) -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::with_connection(conn, sealer)
    }

    fn with_connection(conn: rusqlite::Connection, sealer: Sealer) -> Result<Self> {
        db::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            sealer,
            fts_ready: Mutex::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn seal(&self, ctx: &[u8], text: &str) -> Result<Vec<u8>> {
        Ok(self.sealer.seal(ctx, text.as_bytes())?)
    }

    fn open_text(&self, ctx: &[u8], blob: &[u8]) -> Result<String> {
        let plain = self
            .sealer
            .open(ctx, blob)
            .map_err(|_| StoreError::Sealed)?;
        String::from_utf8(plain.to_vec()).map_err(|_| StoreError::Decode("query text"))
    }

    // Connections.

    /// Saved connections in tree order.
    pub fn connections(&self) -> Result<Vec<ConnectionConfig>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT config FROM connections ORDER BY position, rowid")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?).map_err(|_| StoreError::Decode("connection"))?);
        }
        Ok(out)
    }

    /// Adds or replaces a connection. New connections go to the end.
    pub fn save_connection(&self, config: &ConnectionConfig, now: i64) -> Result<()> {
        let json = serde_json::to_string(config).map_err(|_| StoreError::Decode("connection"))?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO connections (id, position, config, created_at, updated_at)
             VALUES (?1, COALESCE((SELECT MAX(position) + 1 FROM connections), 0), ?2, ?3, ?3)
             ON CONFLICT (id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
            params![config.id.0, json, now],
        )?;
        Ok(())
    }

    /// Removes a connection, its history and its overview snapshots. Its secrets are the
    /// caller's to remove from the SecretStore.
    pub fn delete_connection(&self, id: &ConnectionId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM history WHERE connection_id = ?1", [&id.0])?;
        tx.execute("DELETE FROM snapshots WHERE connection_id = ?1", [&id.0])?;
        tx.execute(
            "UPDATE saved_queries SET connection_id = NULL WHERE connection_id = ?1",
            [&id.0],
        )?;
        tx.execute(
            "UPDATE tabs SET connection_id = NULL WHERE connection_id = ?1",
            [&id.0],
        )?;
        tx.execute("DELETE FROM connections WHERE id = ?1", [&id.0])?;
        tx.commit()?;
        drop(conn);
        self.reset_fts();
        Ok(())
    }

    /// Puts connections in the given order; ids not listed keep their relative order after.
    pub fn reorder_connections(&self, ids: &[ConnectionId]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for (i, id) in ids.iter().enumerate() {
            let pos = i64::try_from(i).unwrap_or(i64::MAX);
            tx.execute(
                "UPDATE connections SET position = ?1 WHERE id = ?2",
                params![pos, id.0],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // Saved queries.

    /// Saved queries, by folder and name.
    pub fn saved_queries(&self) -> Result<Vec<SavedQuery>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, folder, name, connection_id, updated_at FROM saved_queries ORDER BY folder, name",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SavedQuery {
                id: r.get(0)?,
                folder: r.get(1)?,
                name: r.get(2)?,
                connection: r.get::<_, Option<String>>(3)?.map(ConnectionId),
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The text of a saved query.
    pub fn saved_query_text(&self, id: &str) -> Result<Option<String>> {
        let blob: Option<Vec<u8>> = self
            .lock()
            .query_row("SELECT text FROM saved_queries WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        blob.map(|b| self.open_text(SAVED_CTX, &b)).transpose()
    }

    /// Adds or replaces a saved query.
    pub fn save_query(&self, q: &SavedQuery, text: &str) -> Result<()> {
        let sealed = self.seal(SAVED_CTX, text)?;
        self.lock().execute(
            "INSERT INTO saved_queries (id, folder, name, connection_id, text, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (id) DO UPDATE SET folder = excluded.folder, name = excluded.name,
               connection_id = excluded.connection_id, text = excluded.text,
               updated_at = excluded.updated_at",
            params![
                q.id,
                q.folder,
                q.name,
                q.connection.as_ref().map(|c| &c.0),
                sealed,
                q.updated_at
            ],
        )?;
        Ok(())
    }

    /// Removes a saved query.
    pub fn delete_saved_query(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM saved_queries WHERE id = ?1", [id])?;
        Ok(())
    }

    // History.

    /// Records a run. Whether history is on for the connection is the caller's decision.
    pub fn add_history(&self, e: &HistoryEntry) -> Result<i64> {
        let sealed = self.seal(HISTORY_CTX, &e.text)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO history (connection_id, text, executed_at, duration_ms, rows, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                e.connection.0,
                sealed,
                e.executed_at,
                e.duration_ms,
                e.rows,
                e.status.as_str()
            ],
        )?;
        let id = conn.last_insert_rowid();
        if *self
            .fts_ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            conn.execute(
                "INSERT INTO temp.history_fts (rowid, text) VALUES (?1, ?2)",
                params![id, e.text],
            )?;
        }
        Ok(id)
    }

    /// The most recent history of `connection` (all connections when `None`), newest first,
    /// before history id `before` when paging.
    pub fn history(
        &self,
        connection: Option<&ConnectionId>,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, connection_id, text, executed_at, duration_ms, rows, status FROM history
             WHERE (?1 IS NULL OR connection_id = ?1) AND (?2 IS NULL OR id < ?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![connection.map(|c| &c.0), before, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, c, blob, at, dur, n, status) = row?;
            out.push(HistoryEntry {
                id,
                connection: ConnectionId(c),
                text: self.open_text(HISTORY_CTX, &blob)?,
                executed_at: at,
                duration_ms: dur,
                rows: n,
                status: RunStatus::parse(&status),
            });
        }
        Ok(out)
    }

    /// Full-text search over history. Every word must match; the last one may be a prefix.
    pub fn search_history(
        &self,
        connection: Option<&ConnectionId>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>> {
        let Some(fts_query) = fts_query(query) else {
            return self.history(connection, None, limit);
        };
        self.ensure_fts()?;
        let ids: Vec<i64> = {
            let conn = self.lock();
            let mut st = conn.prepare(
                "SELECT h.id FROM temp.history_fts JOIN history h ON h.id = history_fts.rowid
                 WHERE history_fts MATCH ?1 AND (?2 IS NULL OR h.connection_id = ?2)
                 ORDER BY h.id DESC LIMIT ?3",
            )?;
            let rows = st.query_map(params![fts_query, connection.map(|c| &c.0), limit], |r| {
                r.get(0)
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(e) = self.history_entry(id)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn history_entry(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let row = self
            .lock()
            .query_row(
                "SELECT connection_id, text, executed_at, duration_ms, rows, status FROM history WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((c, blob, at, dur, n, status)) = row else {
            return Ok(None);
        };
        Ok(Some(HistoryEntry {
            id,
            connection: ConnectionId(c),
            text: self.open_text(HISTORY_CTX, &blob)?,
            executed_at: at,
            duration_ms: dur,
            rows: n,
            status: RunStatus::parse(&status),
        }))
    }

    /// Builds the in-memory search index from every readable history row.
    fn ensure_fts(&self) -> Result<()> {
        let mut ready = self
            .fts_ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *ready {
            return Ok(());
        }
        let mut conn = self.lock();
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut st = conn.prepare("SELECT id, text FROM history")?;
            let mapped = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.collect::<std::result::Result<_, _>>()?
        };
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM temp.history_fts", [])?;
        {
            let mut ins =
                tx.prepare("INSERT INTO temp.history_fts (rowid, text) VALUES (?1, ?2)")?;
            for (id, blob) in rows {
                // Rows sealed under an older key cannot be indexed; they stay purgeable.
                if let Ok(text) = self.open_text(HISTORY_CTX, &blob) {
                    ins.execute(params![id, text])?;
                }
            }
        }
        tx.commit()?;
        *ready = true;
        Ok(())
    }

    fn reset_fts(&self) {
        *self
            .fts_ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
    }

    /// Deletes history: of one connection, or all of it.
    pub fn purge_history(&self, connection: Option<&ConnectionId>) -> Result<u64> {
        let n = {
            let conn = self.lock();
            let n = conn.execute(
                "DELETE FROM history WHERE (?1 IS NULL OR connection_id = ?1)",
                params![connection.map(|c| &c.0)],
            )?;
            conn.execute("DELETE FROM temp.history_fts", [])?;
            // Overwrite freed pages so purged text does not linger in the file.
            conn.execute_batch("PRAGMA secure_delete = ON; VACUUM;")?;
            n
        };
        self.reset_fts();
        Ok(u64::try_from(n).unwrap_or(0))
    }

    // Settings.

    /// A setting, if set.
    pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let json: Option<String> = self
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        json.map(|j| serde_json::from_str(&j).map_err(|_| StoreError::Decode("setting")))
            .transpose()
    }

    /// Stores a setting.
    pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value).map_err(|_| StoreError::Decode("setting"))?;
        self.lock().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, json],
        )?;
        Ok(())
    }

    // Tabs.

    /// The open tabs, in order.
    pub fn tabs(&self) -> Result<Vec<TabState>> {
        // Rows with their sealed text, decrypted after the connection lock is released.
        let rows: Vec<(TabState, Vec<u8>)> = {
            let conn = self.lock();
            let mut st = conn.prepare(
                "SELECT id, position, connection_id, title, text, cursor, saved_query FROM tabs ORDER BY position",
            )?;
            let mapped = st.query_map([], |r| {
                let tab = TabState {
                    id: r.get(0)?,
                    position: r.get(1)?,
                    connection: r.get::<_, Option<String>>(2)?.map(ConnectionId),
                    title: r.get(3)?,
                    text: String::new(),
                    cursor: r.get(5)?,
                    saved_query: r.get(6)?,
                };
                Ok((tab, r.get(4)?))
            })?;
            mapped.collect::<std::result::Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(mut tab, blob)| {
                tab.text = self.open_text(TAB_CTX, &blob)?;
                Ok(tab)
            })
            .collect()
    }

    /// Saves one tab. Called on every pause in typing, so the text survives a crash.
    pub fn save_tab(&self, t: &TabState) -> Result<()> {
        let sealed = self.seal(TAB_CTX, &t.text)?;
        self.lock().execute(
            "INSERT INTO tabs (id, position, connection_id, title, text, cursor, saved_query)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET position = excluded.position,
               connection_id = excluded.connection_id, title = excluded.title, text = excluded.text,
               cursor = excluded.cursor, saved_query = excluded.saved_query",
            params![
                t.id,
                t.position,
                t.connection.as_ref().map(|c| &c.0),
                t.title,
                sealed,
                t.cursor,
                t.saved_query
            ],
        )?;
        Ok(())
    }

    /// Forgets a closed tab.
    pub fn close_tab(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM tabs WHERE id = ?1", [id])?;
        Ok(())
    }

    // Overview snapshots.

    /// Stores overview metrics and keeps the newest `keep` snapshots of the connection.
    pub fn add_snapshot(
        &self,
        c: &ConnectionId,
        taken_at: i64,
        metrics: &[Metric],
        keep: u32,
    ) -> Result<()> {
        let blob = rmp_serde::to_vec_named(metrics).map_err(|_| StoreError::Decode("snapshot"))?;
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO snapshots (connection_id, taken_at, metrics) VALUES (?1, ?2, ?3)",
            params![c.0, taken_at, blob],
        )?;
        tx.execute(
            "DELETE FROM snapshots WHERE connection_id = ?1 AND rowid NOT IN
               (SELECT rowid FROM snapshots WHERE connection_id = ?1 ORDER BY taken_at DESC LIMIT ?2)",
            params![c.0, keep],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest snapshot taken before `before`, for "since the last snapshot" comparisons.
    pub fn snapshot_before(
        &self,
        c: &ConnectionId,
        before: i64,
    ) -> Result<Option<(i64, Vec<Metric>)>> {
        let row: Option<(i64, Vec<u8>)> = self
            .lock()
            .query_row(
                "SELECT taken_at, metrics FROM snapshots WHERE connection_id = ?1 AND taken_at < ?2
                 ORDER BY taken_at DESC LIMIT 1",
                params![c.0, before],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(at, blob)| {
            rmp_serde::from_slice(&blob)
                .map(|m| (at, m))
                .map_err(|_| StoreError::Decode("snapshot"))
        })
        .transpose()
    }
}

/// Turns user input into an FTS5 query: every word quoted (so FTS syntax in the input is
/// literal), all required, the last one a prefix. `None` for input without words.
fn fts_query(input: &str) -> Option<String> {
    let words: Vec<String> = input
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\""))
        .collect();
    let (last, rest) = words.split_last()?;
    let mut q = rest.join(" ");
    if !q.is_empty() {
        q.push(' ');
    }
    q.push_str(last);
    q.push('*');
    Some(q)
}

#[cfg(test)]
mod tests;
