//! Grid edits as Redis commands, on the one key a result came from: a hash's fields, a set's or
//! sorted set's members, a list's entries by position, or a string's value. They run together
//! in MULTI/EXEC, so all of them apply or none.
//!
//! The analyzer names the key and its type in the result's [`TableRef`]: the type as the
//! schema, the key as the table.

use crate::lang::quote;
use crate::session::RedisSession;
use async_trait::async_trait;
use treasury_core::driver::Edit;
use treasury_core::edit::{Changeset, RowChange, TableRef};
use treasury_core::value::Value;
use treasury_core::{Error, ErrorKind, Result};

/// A cell's value as a word of a command. Redis has no NULL.
fn word(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => {
            return Err(Error::config(
                None,
                "Redis has no NULL. Type a value, or delete the entry.",
            ));
        }
        Value::Text(s) | Value::Decimal(s) => s.clone(),
        Value::I64(n) => n.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        other => treasury_core::format::full_text(other),
    })
}

fn value_of<'a>(values: &'a [(String, Value)], column: &str) -> Option<&'a Value> {
    values.iter().find(|(c, _)| c == column).map(|(_, v)| v)
}

fn refuse(message: &str) -> Error {
    Error::new(ErrorKind::Guard, message)
}

/// The commands, as words, that `changes` become.
fn commands(changes: &Changeset) -> Result<Vec<Vec<String>>> {
    let key = changes.table.table.clone();
    let kind = changes.table.schema.as_deref().unwrap_or("");
    let first = |k: &[Value]| -> Result<String> {
        k.first()
            .map(word)
            .transpose()?
            .ok_or_else(|| Error::internal("a change without its key"))
    };
    let mut out: Vec<Vec<String>> = Vec::new();
    for change in &changes.changes {
        let words: Vec<Vec<String>> = match (kind, change) {
            ("hash", RowChange::Update { key: k, set }) => {
                if value_of(set, "field").is_some() {
                    return Err(refuse(
                        "A field's name cannot be changed in place. Delete the row and add one \
                         with the new name.",
                    ));
                }
                let value = value_of(set, "value").ok_or_else(|| refuse("Change the value."))?;
                vec![vec!["HSET".into(), key.clone(), first(k)?, word(value)?]]
            }
            ("hash", RowChange::Delete { key: k }) => {
                vec![vec!["HDEL".into(), key.clone(), first(k)?]]
            }
            ("hash", RowChange::Insert { values }) => {
                let (Some(f), Some(v)) = (value_of(values, "field"), value_of(values, "value"))
                else {
                    return Err(refuse("A new row of a hash needs a field and a value."));
                };
                vec![vec!["HSET".into(), key.clone(), word(f)?, word(v)?]]
            }
            ("set", RowChange::Update { key: k, set }) => {
                let new = value_of(set, "member").ok_or_else(|| refuse("Change the member."))?;
                vec![
                    vec!["SREM".into(), key.clone(), first(k)?],
                    vec!["SADD".into(), key.clone(), word(new)?],
                ]
            }
            ("set", RowChange::Delete { key: k }) => {
                vec![vec!["SREM".into(), key.clone(), first(k)?]]
            }
            ("set", RowChange::Insert { values }) => {
                let m = value_of(values, "member")
                    .ok_or_else(|| refuse("A new row of a set needs a member."))?;
                vec![vec!["SADD".into(), key.clone(), word(m)?]]
            }
            ("zset", RowChange::Update { key: k, set }) => {
                if value_of(set, "member").is_some() {
                    return Err(refuse(
                        "A member's name cannot be changed in place. Delete the row and add one \
                         with the new name and its score.",
                    ));
                }
                let score = value_of(set, "score").ok_or_else(|| refuse("Change the score."))?;
                vec![vec!["ZADD".into(), key.clone(), word(score)?, first(k)?]]
            }
            ("zset", RowChange::Delete { key: k }) => {
                vec![vec!["ZREM".into(), key.clone(), first(k)?]]
            }
            ("zset", RowChange::Insert { values }) => {
                let (Some(m), Some(s)) = (value_of(values, "member"), value_of(values, "score"))
                else {
                    return Err(refuse(
                        "A new row of a sorted set needs a member and a score.",
                    ));
                };
                vec![vec!["ZADD".into(), key.clone(), word(s)?, word(m)?]]
            }
            ("list", RowChange::Update { key: k, set }) => {
                let value = value_of(set, "value").ok_or_else(|| refuse("Change the value."))?;
                vec![vec!["LSET".into(), key.clone(), first(k)?, word(value)?]]
            }
            ("list", RowChange::Delete { .. }) => {
                return Err(refuse(
                    "Redis cannot remove a list entry by its position in one step. Use LREM in \
                     the editor to remove entries by their value.",
                ));
            }
            ("list", RowChange::Insert { values }) => {
                let v = value_of(values, "value")
                    .ok_or_else(|| refuse("A new row of a list needs a value."))?;
                vec![vec!["RPUSH".into(), key.clone(), word(v)?]]
            }
            ("string", RowChange::Update { set, .. }) => {
                let value = value_of(set, "value").ok_or_else(|| refuse("Change the value."))?;
                // The key keeps its expiry.
                vec![vec![
                    "SET".into(),
                    key.clone(),
                    word(value)?,
                    "KEEPTTL".into(),
                ]]
            }
            ("string", RowChange::Delete { .. }) => vec![vec!["DEL".into(), key.clone()]],
            ("string", RowChange::Insert { .. }) => {
                return Err(refuse(
                    "A string holds one value; change it instead of adding a row.",
                ));
            }
            _ => return Err(refuse("Rows of this result cannot be edited.")),
        };
        out.extend(words);
    }
    Ok(out)
}

#[async_trait]
impl Edit for RedisSession {
    fn render(&self, changes: &Changeset) -> Result<Vec<String>> {
        Ok(commands(changes)?
            .iter()
            .map(|w| {
                w.iter()
                    .enumerate()
                    .map(|(i, word)| if i == 0 { word.clone() } else { quote(word) })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect())
    }

    async fn apply(&self, changes: &Changeset) -> Result<u64> {
        let commands = commands(changes)?;
        let mut pipe = redis::pipe();
        pipe.atomic();
        for words in &commands {
            let mut cmd = redis::cmd(&words[0]);
            for w in &words[1..] {
                cmd.arg(w);
            }
            pipe.add_command(cmd).ignore();
        }
        let mut conn = self.conn().await;
        let _: () = conn.pipeline(&pipe).await.map_err(|e| self.map(&e))?;
        Ok(u64::try_from(changes.changes.len()).unwrap_or(u64::MAX))
    }

    /// What tells a result's rows apart, by the type of its key.
    async fn row_key(&self, table: &TableRef) -> Result<Vec<String>> {
        Ok(match table.schema.as_deref() {
            Some("hash") => vec!["field".into()],
            Some("set" | "zset") => vec!["member".into()],
            Some("list") => vec!["index".into()],
            Some("string") => vec!["value".into()],
            _ => Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(kind: &str, list: Vec<RowChange>) -> Changeset {
        Changeset {
            table: TableRef {
                schema: Some(kind.into()),
                table: "user:1".into(),
            },
            key_columns: vec![],
            changes: list,
        }
    }

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn each_type_has_its_commands() {
        let c = changes(
            "hash",
            vec![
                RowChange::Update {
                    key: vec![t("name")],
                    set: vec![("value".into(), t("Ada Lovelace"))],
                },
                RowChange::Delete {
                    key: vec![t("old")],
                },
                RowChange::Insert {
                    values: vec![("field".into(), t("lang")), ("value".into(), t("rust"))],
                },
            ],
        );
        assert_eq!(
            commands(&c).unwrap(),
            [
                vec!["HSET", "user:1", "name", "Ada Lovelace"],
                vec!["HDEL", "user:1", "old"],
                vec!["HSET", "user:1", "lang", "rust"],
            ]
        );
        let z = changes(
            "zset",
            vec![RowChange::Update {
                key: vec![t("a")],
                set: vec![("score".into(), Value::F64(2.5))],
            }],
        );
        assert_eq!(commands(&z).unwrap(), [vec!["ZADD", "user:1", "2.5", "a"]]);
        let s = changes(
            "string",
            vec![RowChange::Update {
                key: vec![t("old")],
                set: vec![("value".into(), t("new"))],
            }],
        );
        assert_eq!(
            commands(&s).unwrap(),
            [vec!["SET", "user:1", "new", "KEEPTTL"]]
        );
    }

    #[test]
    fn what_redis_cannot_do_in_one_step_is_refused_with_why() {
        let l = changes(
            "list",
            vec![RowChange::Delete {
                key: vec![Value::I64(3)],
            }],
        );
        assert!(commands(&l).unwrap_err().message.contains("LREM"));
        let h = changes(
            "hash",
            vec![RowChange::Update {
                key: vec![t("a")],
                set: vec![("field".into(), t("b"))],
            }],
        );
        assert!(commands(&h).is_err());
        let n = changes(
            "hash",
            vec![RowChange::Update {
                key: vec![t("a")],
                set: vec![("value".into(), Value::Null)],
            }],
        );
        assert!(commands(&n).unwrap_err().message.contains("NULL"));
    }
}
