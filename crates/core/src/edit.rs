//! Row edits collected in the grid and applied together.

use crate::value::Value;
use serde::{Deserialize, Serialize};

/// The table being edited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    /// Schema, database or keyspace.
    pub schema: Option<String>,
    /// Table or collection.
    pub table: String,
}

/// One change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowChange {
    /// Set columns of the row identified by `key`.
    Update {
        /// Values of the key columns, in [`Changeset::key_columns`] order.
        key: Vec<Value>,
        /// The columns to change and their new values.
        set: Vec<(String, Value)>,
    },
    /// Insert a row.
    Insert {
        /// Column names and values.
        values: Vec<(String, Value)>,
    },
    /// Delete the row identified by `key`.
    Delete {
        /// Values of the key columns.
        key: Vec<Value>,
    },
}

/// When a key expires: the engines that have expiry (Redis and Valkey) keep it per key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "seconds", rename_all = "snake_case")]
pub enum Expiry {
    /// It is kept until it is deleted.
    Never,
    /// It is deleted this many seconds from now.
    After(u64),
}

/// Pending edits to one table, applied in one transaction where the engine has transactions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Changeset {
    /// The table.
    pub table: TableRef,
    /// The primary or unique key that identifies rows.
    pub key_columns: Vec<String>,
    /// The changes, in the order they are applied.
    pub changes: Vec<RowChange>,
    /// A new expiry for the whole key, applied after the changes. Only a driver whose
    /// [`Edit::expiry`](crate::driver::Edit::expiry) reports one is given it.
    #[serde(default)]
    pub expire: Option<Expiry>,
}

impl Changeset {
    /// Checks that every change identifies its row by the full key.
    pub fn validate(&self) -> crate::Result<()> {
        if self.key_columns.is_empty() {
            return Err(crate::Error::new(
                crate::ErrorKind::Guard,
                "This result has no primary or unique key, so its rows cannot be edited.",
            ));
        }
        for change in &self.changes {
            let key = match change {
                RowChange::Update { key, set } => {
                    if set.is_empty() {
                        return Err(crate::Error::internal("an update without columns"));
                    }
                    key
                }
                RowChange::Delete { key } => key,
                RowChange::Insert { .. } => continue,
            };
            if key.len() != self.key_columns.len() {
                return Err(crate::Error::internal(
                    "a row change does not carry every key column",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_need_a_key() {
        let mut cs = Changeset {
            table: TableRef {
                schema: None,
                table: "t".into(),
            },
            key_columns: vec![],
            changes: vec![RowChange::Delete {
                key: vec![Value::I64(1)],
            }],
            expire: None,
        };
        assert!(cs.validate().is_err());
        cs.key_columns = vec!["id".into()];
        cs.validate().unwrap();
        cs.changes.push(RowChange::Update {
            key: vec![],
            set: vec![("a".into(), Value::Null)],
        });
        assert!(cs.validate().is_err());
    }
}
