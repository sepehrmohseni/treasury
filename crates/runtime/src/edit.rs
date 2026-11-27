//! Editing a result's rows in place: the table and key behind a result set, the grid's pending
//! edits turned into a [`Changeset`], and applying it on the connection that produced the rows.
//!
//! Rows are identified by the key values the server sent, never by what the grid displays, so
//! formatting cannot change which row an edit reaches.

use crate::{Running, Runtime, guard_error};
use std::collections::BTreeMap;
use treasury_core::config::Family;
use treasury_core::edit::{Changeset, Expiry, RowChange, TableRef};
use treasury_core::exec::ValueKind;
use treasury_core::{Error, Result, Value};
use treasury_ipc::{CellEdit, EditInfo, RowEdits};

/// The table and key behind one result set.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    set: usize,
    table: TableRef,
    key_columns: Vec<String>,
    /// Where each key column is in the result.
    key_at: Vec<usize>,
    /// The table column behind each result column; `None` for computed ones.
    columns: Vec<Option<String>>,
    /// Result columns that name a row's entry and cannot change in place (they can be set on a
    /// new row).
    locked: Vec<usize>,
    kinds: Vec<ValueKind>,
    /// The rows are documents, whose fields take any type.
    documents: bool,
    /// The rows come from a key with an expiry, which can be changed with them.
    expires: bool,
}

/// `schema.table`, for messages. A Redis result's "schema" is its key's type: `the hash user:1`.
fn display(t: &TableRef, family: Family) -> String {
    match (&t.schema, family) {
        (Some(kind), Family::KeyValue) => format!("the {kind} {}", t.table),
        (Some(s), _) => format!("{s}.{}", t.table),
        (None, _) => t.table.clone(),
    }
}

/// Where `key` is among `columns`: the same name, or else the only one that differs just in
/// case (MySQL and SQL Server compare column names without case).
fn position(columns: &[Option<String>], key: &str) -> Option<usize> {
    if let Some(i) = columns.iter().position(|c| c.as_deref() == Some(key)) {
        return Some(i);
    }
    let mut loose = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(key)));
    match (loose.next(), loose.next()) {
        (Some((i, _)), None) => Some(i),
        _ => None,
    }
}

/// The value typed into a cell, checked against the column's kind. `None` is NULL. Kinds the
/// server parses better than Treasury could (dates, decimals, JSON) go as text.
pub(crate) fn parse(
    text: Option<&str>,
    kind: ValueKind,
    column: &str,
    documents: bool,
) -> Result<Value> {
    let Some(t) = text else {
        return Ok(Value::Null);
    };
    let bad = |what: &str| Error::config(Some(column), format!("“{t}” is not {what}."));
    Ok(match kind {
        ValueKind::Integer => Value::I64(t.trim().parse().map_err(|_| bad("a whole number"))?),
        ValueKind::Float => Value::F64(t.trim().parse().map_err(|_| bad("a number"))?),
        ValueKind::Bool => match t.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "1" => Value::Bool(true),
            "false" | "f" | "no" | "0" => Value::Bool(false),
            _ => return Err(bad("true or false")),
        },
        ValueKind::Text => Value::Text(t.to_owned()),
        // A document's fields hold any type: what is typed is read as Extended JSON when it is
        // JSON (`42`, `{"a": 1}`, `[1, 2]`, `{"$oid": "..."}`), and kept as text otherwise. A
        // date can be typed as it is shown.
        ValueKind::Timestamp | ValueKind::TimestampTz
            if documents && !t.trim().starts_with('{') =>
        {
            Value::Json(treasury_core::value::Json {
                text: serde_json::json!({ "$date": t.trim() }).to_string(),
                binary: false,
            })
        }
        _ if documents && serde_json::from_str::<serde_json::Value>(t).is_ok() => {
            Value::Json(treasury_core::value::Json {
                text: t.trim().to_owned(),
                binary: false,
            })
        }
        _ => Value::Text(t.to_owned()),
    })
}

impl Runtime {
    /// Whether the rows of result set `set` can be edited, and which of its columns.
    pub async fn edit_info(&self, rid: u64, set: usize) -> Result<EditInfo> {
        let r = self.running(rid)?;
        let live = self.live(&r.connection).await?;
        let family = live.config.engine.family();
        Ok(match self.target(&r, set).await? {
            Ok(t) => EditInfo {
                table: Some(display(&t.table, family)),
                columns: t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| c.is_some() && !t.locked.contains(&i))
                    .collect(),
                reason: None,
                expiry: match (t.expires, live.session.edit()) {
                    (true, Some(e)) => e.expiry(&t.table).await?,
                    _ => None,
                },
            },
            Err(reason) => EditInfo {
                table: None,
                columns: Vec::new(),
                reason: Some(reason),
                expiry: None,
            },
        })
    }

    /// The statements `edits` would run, for the user to review.
    pub async fn edit_review(&self, rid: u64, set: usize, edits: &RowEdits) -> Result<Vec<String>> {
        let r = self.running(rid)?;
        let changes = self.changeset(&r, set, edits).await?;
        let live = self.live(&r.connection).await?;
        live.session
            .edit()
            .ok_or_else(|| Error::unsupported("This connection cannot edit rows."))?
            .render(&changes)
    }

    /// Applies `edits` in one transaction and returns the rows changed. A read-only connection
    /// refuses them until writes are unlocked.
    pub async fn edit_apply(&self, rid: u64, set: usize, edits: &RowEdits) -> Result<u64> {
        let r = self.running(rid)?;
        let live = self.live(&r.connection).await?;
        self.relock_if_expired(&live).await?;
        if live.read_only() {
            return Err(guard_error(
                "This connection is read-only. Unlock writes to save changes.",
            ));
        }
        let changes = self.changeset(&r, set, edits).await?;
        live.session
            .edit()
            .ok_or_else(|| Error::unsupported("This connection cannot edit rows."))?
            .apply(&changes)
            .await
    }

    /// The table and key behind result set `set`, or why its rows cannot be edited. Found once
    /// per result and kept.
    async fn target(&self, r: &Running, set: usize) -> Result<std::result::Result<Target, String>> {
        if let Some(t) = r
            .edit_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .filter(|t| t.set == set)
        {
            return Ok(Ok(t));
        }
        let live = self.live(&r.connection).await?;
        let status = r.execution.status();
        let Some(s) = status.sets.get(set) else {
            return Ok(Err("This result has not arrived yet.".into()));
        };
        if s.columns.is_empty() {
            return Ok(Err("This statement returned no rows.".into()));
        }
        if live.session.edit().is_none() {
            return Ok(Err("Rows of this connection cannot be edited.".into()));
        }
        let source = match live.session.analyzer().edit_source(&r.text) {
            Ok(s) => s,
            Err(reason) => return Ok(Err(reason)),
        };
        let names: Vec<String> = s.columns.iter().map(|c| c.name.clone()).collect();
        let Some(columns) = source.columns(&names) else {
            return Ok(Err(
                "Treasury could not match the result's columns to the table's.".into(),
            ));
        };
        let finder = live.meta.edit().or_else(|| live.session.edit());
        let Some(finder) = finder else {
            return Ok(Err("Rows of this connection cannot be edited.".into()));
        };
        let key_columns = finder.row_key(&source.table).await?;
        if key_columns.is_empty() {
            return Ok(Err(format!(
                "{} has no primary key or unique key, so Treasury cannot tell its rows apart.",
                display(&source.table, live.config.engine.family())
            )));
        }
        let mut key_at = Vec::with_capacity(key_columns.len());
        for k in &key_columns {
            match position(&columns, k) {
                Some(i) => key_at.push(i),
                None => {
                    return Ok(Err(format!(
                        "Add the key column {k} to the SELECT to edit these rows."
                    )));
                }
            }
        }
        let family = live.config.engine.family();
        let documents = family == Family::Document;
        // A document's `_id`, a hash's field, a set's member and a list entry's position name
        // what they belong to and cannot change in place; a Redis string's one column is its
        // value, which can.
        let fixed_keys = match family {
            Family::Document => true,
            Family::KeyValue => source.table.schema.as_deref() != Some("string"),
            _ => false,
        };
        let locked = if fixed_keys {
            key_at.clone()
        } else {
            Vec::new()
        };
        // Asked on the session that applies the edits, which has the key's database selected.
        let expires = match live.session.edit() {
            Some(e) => e.expiry(&source.table).await?.is_some(),
            None => false,
        };
        let target = Target {
            set,
            table: source.table,
            key_columns,
            key_at,
            columns,
            locked,
            kinds: s.columns.iter().map(|c| c.kind).collect(),
            documents,
            expires,
        };
        *r.edit_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target.clone());
        Ok(Ok(target))
    }

    /// `edits` as a changeset on the table behind result set `set`.
    async fn changeset(&self, r: &Running, set: usize, edits: &RowEdits) -> Result<Changeset> {
        let target = self.target(r, set).await?.map_err(guard_error)?;
        if edits.cells.is_empty()
            && edits.deletes.is_empty()
            && edits.inserts.is_empty()
            && edits.expire.is_none()
        {
            return Err(guard_error("There are no changes to save."));
        }
        if edits.expire.is_some() && !target.expires {
            return Err(guard_error(
                "These rows do not come from a key that expires.",
            ));
        }
        if edits.expire == Some(Expiry::After(0)) {
            return Err(guard_error(
                "An expiry of 0 seconds deletes the key. Give it at least 1 second.",
            ));
        }
        let value_of = |column: usize, text: Option<&str>| -> Result<(String, Value)> {
            let Some(Some(name)) = target.columns.get(column) else {
                return Err(guard_error(
                    "That column is computed by the statement, so it cannot be edited.",
                ));
            };
            let kind = target.kinds.get(column).copied().unwrap_or(ValueKind::Text);
            Ok((name.clone(), parse(text, kind, name, target.documents)?))
        };
        let mut deletes = edits.deletes.clone();
        deletes.sort_unstable();
        deletes.dedup();
        // Cells of a row that is being deleted do not matter.
        let mut by_row: BTreeMap<u64, Vec<&CellEdit>> = BTreeMap::new();
        for c in edits
            .cells
            .iter()
            .filter(|c| deletes.binary_search(&c.row).is_err())
        {
            by_row.entry(c.row).or_default().push(c);
        }
        let mut changes = Vec::with_capacity(by_row.len() + deletes.len());
        for (row, cells) in by_row {
            let key = self.key_of(r, &target, set, row).await?;
            let mut values: Vec<(String, Value)> = Vec::new();
            for c in cells {
                if target.locked.contains(&c.column) {
                    return Err(guard_error(
                        "That column names the entry, so it cannot change in place. Delete the row \
                         and add one with the new name.",
                    ));
                }
                let (column, value) = value_of(c.column, c.value.as_deref())?;
                // The last edit of a cell is the one that counts.
                match values.iter_mut().find(|(n, _)| *n == column) {
                    Some(existing) => existing.1 = value,
                    None => values.push((column, value)),
                }
            }
            changes.push(RowChange::Update { key, set: values });
        }
        for row in deletes {
            let key = self.key_of(r, &target, set, row).await?;
            changes.push(RowChange::Delete { key });
        }
        for row in &edits.inserts {
            let mut values: Vec<(String, Value)> = Vec::new();
            for (column, text) in row {
                let (column, value) = value_of(*column, text.as_deref())?;
                match values.iter_mut().find(|(n, _)| *n == column) {
                    Some(existing) => existing.1 = value,
                    None => values.push((column, value)),
                }
            }
            changes.push(RowChange::Insert { values });
        }
        Ok(Changeset {
            table: target.table,
            key_columns: target.key_columns,
            changes,
            expire: edits.expire,
        })
    }

    /// The key values of row `row`, as the server sent them.
    async fn key_of(
        &self,
        r: &Running,
        target: &Target,
        set: usize,
        row: u64,
    ) -> Result<Vec<Value>> {
        let w = r.execution.window(set, row, 1).await?;
        let values = w
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| guard_error("That row is no longer in the result. Run it again."))?;
        target
            .key_at
            .iter()
            .map(|&i| {
                values
                    .get(i)
                    .cloned()
                    .ok_or_else(|| Error::internal("a key column is missing from the row"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_fields_take_json() {
        let json = |t: &str, k: ValueKind| match parse(Some(t), k, "f", true).unwrap() {
            Value::Json(j) => Some(j.text),
            _ => None,
        };
        assert_eq!(json("42", ValueKind::Any).as_deref(), Some("42"));
        assert_eq!(
            json(r#"{"a": [1, 2]}"#, ValueKind::Document).as_deref(),
            Some(r#"{"a": [1, 2]}"#)
        );
        assert_eq!(
            json("2026-11-27T10:00:00Z", ValueKind::Timestamp).as_deref(),
            Some(r#"{"$date":"2026-11-27T10:00:00Z"}"#)
        );
        // Not JSON, or a text field: kept as text.
        assert_eq!(
            parse(Some("hello"), ValueKind::Any, "f", true).unwrap(),
            Value::Text("hello".into())
        );
        assert_eq!(
            parse(Some("42"), ValueKind::Text, "f", true).unwrap(),
            Value::Text("42".into())
        );
        // SQL columns are not read as JSON.
        assert_eq!(
            parse(Some("[1]"), ValueKind::List, "f", false).unwrap(),
            Value::Text("[1]".into())
        );
    }

    #[test]
    fn typed_values_are_checked_against_their_column() {
        assert_eq!(
            parse(None, ValueKind::Integer, "n", false).unwrap(),
            Value::Null
        );
        assert_eq!(
            parse(Some(" 42 "), ValueKind::Integer, "n", false).unwrap(),
            Value::I64(42)
        );
        assert!(parse(Some("4.2"), ValueKind::Integer, "n", false).is_err());
        assert_eq!(
            parse(Some("no"), ValueKind::Bool, "b", false).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            parse(Some("2026-11-26"), ValueKind::Date, "d", false).unwrap(),
            Value::Text("2026-11-26".into())
        );
        // Text keeps its spaces; an empty cell is an empty string, not NULL.
        assert_eq!(
            parse(Some(" "), ValueKind::Text, "t", false).unwrap(),
            Value::Text(" ".into())
        );
    }

    #[test]
    fn key_columns_match_exactly_then_without_case() {
        let cols = vec![Some("Id".to_owned()), None, Some("name".to_owned())];
        assert_eq!(position(&cols, "Id"), Some(0));
        assert_eq!(position(&cols, "id"), Some(0));
        assert_eq!(position(&cols, "missing"), None);
        let twice = vec![Some("a".to_owned()), Some("A".to_owned())];
        assert_eq!(position(&twice, "a"), Some(0));
        assert_eq!(position(&twice, "x"), None);
    }
}
