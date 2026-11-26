//! Grid edits as update, insert and delete commands. On a replica set they run in one
//! transaction; a standalone server has none, so they run in order and stop at the first failure,
//! and the error says how many were applied.

use crate::session::MongoSession;
use crate::value::to_bson;
use async_trait::async_trait;
use mongodb::bson::{Bson, Document, doc};
use treasury_core::driver::Edit;
use treasury_core::edit::{Changeset, RowChange};
use treasury_core::{Error, ErrorKind, Result};

/// MongoDB's "transactions need a replica set" error.
const ILLEGAL_OPERATION: i32 = 20;

struct Planned {
    command: Document,
    one: bool,
}

fn plan(changes: &Changeset) -> Result<Vec<Planned>> {
    let coll = &changes.table.table;
    let key = |values: &[treasury_core::Value]| -> Document {
        changes
            .key_columns
            .iter()
            .zip(values)
            .map(|(k, v)| (k.clone(), to_bson(v)))
            .collect()
    };
    changes
        .changes
        .iter()
        .map(|c| {
            Ok(match c {
                RowChange::Update { key: k, set } => {
                    let fields: Document = set.iter().map(|(f, v)| (f.clone(), to_bson(v))).collect();
                    Planned {
                        command: doc! {"update": coll, "updates": [{"q": key(k), "u": {"$set": fields}, "multi": false}]},
                        one: true,
                    }
                }
                RowChange::Insert { values } => {
                    let d: Document = values.iter().map(|(f, v)| (f.clone(), to_bson(v))).collect();
                    Planned {
                        command: doc! {"insert": coll, "documents": [d]},
                        one: false,
                    }
                }
                RowChange::Delete { key: k } => Planned {
                    command: doc! {"delete": coll, "deletes": [{"q": key(k), "limit": 1}]},
                    one: true,
                },
            })
        })
        .collect()
}

fn affected(reply: &Document) -> u64 {
    match reply.get("n") {
        Some(Bson::Int32(n)) => u64::try_from(*n).unwrap_or(0),
        Some(Bson::Int64(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    }
}

fn check(reply: &Document, one: bool) -> Result<u64> {
    if let Ok(errors) = reply.get_array("writeErrors")
        && let Some(e) = errors.first().and_then(Bson::as_document)
    {
        return Err(Error::new(
            ErrorKind::Server,
            e.get_str("errmsg")
                .unwrap_or("The write failed.")
                .to_owned(),
        ));
    }
    let n = affected(reply);
    if one && n != 1 {
        return Err(Error::new(
            ErrorKind::Guard,
            "Nothing was saved: a document you edited was changed or deleted since it was loaded. Reload and edit again.",
        ));
    }
    Ok(n)
}

#[async_trait]
impl Edit for MongoSession {
    fn render(&self, changes: &Changeset) -> Result<Vec<String>> {
        changes.validate()?;
        Ok(plan(changes)?
            .into_iter()
            .map(|p| {
                serde_json::to_string(&Bson::Document(p.command).into_relaxed_extjson())
                    .unwrap_or_default()
            })
            .collect())
    }

    async fn apply(&self, changes: &Changeset) -> Result<u64> {
        changes.validate()?;
        self.alive()?;
        let planned = plan(changes)?;
        let db = self
            .client
            .database(changes.table.schema.as_deref().unwrap_or(&self.database));
        let mut session = self
            .client
            .start_session()
            .await
            .map_err(|e| self.map(&e))?;
        match session.start_transaction().await {
            Ok(()) => {}
            Err(e) => return Err(self.map(&e)),
        }
        let mut total = 0;
        let mut in_transaction = true;
        for (i, p) in planned.iter().enumerate() {
            let reply = if in_transaction {
                match db
                    .run_command(p.command.clone())
                    .session(&mut session)
                    .await
                {
                    Err(e)
                        if i == 0
                            && matches!(e.kind.as_ref(), mongodb::error::ErrorKind::Command(c) if c.code == ILLEGAL_OPERATION) =>
                    {
                        // A standalone server: no transactions.
                        in_transaction = false;
                        let _ = session.abort_transaction().await;
                        db.run_command(p.command.clone()).await
                    }
                    other => other,
                }
            } else {
                db.run_command(p.command.clone()).await
            };
            let result = reply
                .map_err(|e| self.map(&e))
                .and_then(|r| check(&r, p.one));
            match result {
                Ok(n) => total += n,
                Err(e) => {
                    if in_transaction {
                        let _ = session.abort_transaction().await;
                        return Err(e);
                    }
                    return Err(Error::new(
                        e.kind.clone(),
                        format!(
                            "{} This server has no transactions, so the {i} change(s) before it were saved.",
                            e.message
                        ),
                    ));
                }
            }
        }
        if in_transaction {
            session
                .commit_transaction()
                .await
                .map_err(|e| self.map(&e))?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use treasury_core::Value;
    use treasury_core::edit::TableRef;

    #[test]
    fn edits_become_commands() {
        let cs = Changeset {
            table: TableRef {
                schema: None,
                table: "orders".into(),
            },
            key_columns: vec!["_id".into()],
            changes: vec![
                RowChange::Update {
                    key: vec![Value::ObjectId([1; 12])],
                    set: vec![("status".into(), Value::Text("paid".into()))],
                },
                RowChange::Delete {
                    key: vec![Value::I64(2)],
                },
            ],
        };
        let p = plan(&cs).unwrap();
        assert_eq!(p[0].command.get_str("update").unwrap(), "orders");
        assert!(p[0].one);
        let del = p[1].command.get_array("deletes").unwrap()[0]
            .as_document()
            .unwrap();
        assert_eq!(del.get_i32("limit").unwrap(), 1);
        assert!(check(&doc! {"n": 0, "ok": 1}, true).is_err());
        assert_eq!(check(&doc! {"n": 1, "ok": 1}, true).unwrap(), 1);
    }
}
