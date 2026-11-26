//! Row edits from the grid, applied in one transaction with bind parameters.
//!
//! When the user already has a transaction open, the edits join it under a savepoint instead of
//! starting and committing one of their own.

use crate::session::{MySqlSession, cell};
use crate::{error, value};
use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use treasury_core::driver::Edit;
use treasury_core::edit::{Changeset, RowChange, TableRef};
use treasury_core::{Error, ErrorKind, Result, Value};
use treasury_sql::render;

const SAVEPOINT: &str = "treasury_edit";
/// MySQL's "SAVEPOINT does not exist": no transaction was open to hold it.
const ER_SP_DOES_NOT_EXIST: u16 = 1305;

struct Planned {
    sql: String,
    review: String,
    params: Vec<Value>,
    one_row: bool,
}

fn plan(d: treasury_core::driver::SqlDialect, changes: &Changeset) -> Vec<Planned> {
    let table = render::table(d, changes.table.schema.as_deref(), &changes.table.table);
    let key = |key: &[Value], params: &mut Vec<Value>| -> (String, String) {
        let mut sql = Vec::new();
        let mut review = Vec::new();
        for (c, v) in changes.key_columns.iter().zip(key) {
            let col = render::ident(d, c);
            if v.is_null() {
                sql.push(format!("{col} IS NULL"));
                review.push(format!("{col} IS NULL"));
            } else {
                params.push(v.clone());
                sql.push(format!("{col} = ?"));
                review.push(format!("{col} = {}", render::literal(d, v)));
            }
        }
        (sql.join(" AND "), review.join(" AND "))
    };
    changes
        .changes
        .iter()
        .map(|change| {
            let mut params = Vec::new();
            let (sql, review, one_row) = match change {
                RowChange::Update { key: k, set } => {
                    let mut sets = Vec::new();
                    let mut shown = Vec::new();
                    for (c, v) in set {
                        params.push(v.clone());
                        sets.push(format!("{} = ?", render::ident(d, c)));
                        shown.push(format!(
                            "{} = {}",
                            render::ident(d, c),
                            render::literal(d, v)
                        ));
                    }
                    let (w, wr) = key(k, &mut params);
                    (
                        format!("UPDATE {table} SET {} WHERE {w}", sets.join(", ")),
                        format!("UPDATE {table} SET {} WHERE {wr}", shown.join(", ")),
                        true,
                    )
                }
                RowChange::Insert { values } if values.is_empty() => {
                    let sql = format!("INSERT INTO {table} () VALUES ()");
                    (sql.clone(), sql, false)
                }
                RowChange::Insert { values } => {
                    let cols = values
                        .iter()
                        .map(|(c, _)| render::ident(d, c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let literals = values
                        .iter()
                        .map(|(_, v)| render::literal(d, v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    params.extend(values.iter().map(|(_, v)| v.clone()));
                    let holders = vec!["?"; values.len()].join(", ");
                    (
                        format!("INSERT INTO {table} ({cols}) VALUES ({holders})"),
                        format!("INSERT INTO {table} ({cols}) VALUES ({literals})"),
                        false,
                    )
                }
                RowChange::Delete { key: k } => {
                    let (w, wr) = key(k, &mut params);
                    (
                        format!("DELETE FROM {table} WHERE {w}"),
                        format!("DELETE FROM {table} WHERE {wr}"),
                        true,
                    )
                }
            };
            Planned {
                sql,
                review: format!("{review};"),
                params,
                one_row,
            }
        })
        .collect()
}

#[async_trait]
impl Edit for MySqlSession {
    fn render(&self, changes: &Changeset) -> Result<Vec<String>> {
        changes.validate()?;
        Ok(plan(self.dialect(), changes)
            .into_iter()
            .map(|p| p.review)
            .collect())
    }

    async fn apply(&self, changes: &Changeset) -> Result<u64> {
        changes.validate()?;
        let statements = plan(self.dialect(), changes);
        let (host, port) = (self.host.clone(), self.port);
        self.with_conn(move |conn| {
            Box::pin(async move {
                let map =
                    |e: &mysql_async::Error, sql: Option<&str>| error::map(e, &host, port, sql);
                // A savepoint that cannot be rolled back to means no transaction was open.
                conn.query_drop(format!("SAVEPOINT {SAVEPOINT}"))
                    .await
                    .map_err(|e| map(&e, None))?;
                let nested = match conn
                    .query_drop(format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                    .await
                {
                    Ok(()) => true,
                    Err(mysql_async::Error::Server(e)) if e.code == ER_SP_DOES_NOT_EXIST => {
                        conn.query_drop("START TRANSACTION")
                            .await
                            .map_err(|e| map(&e, None))?;
                        false
                    }
                    Err(e) => return Err(map(&e, None)),
                };
                let mut result: Result<u64> = Ok(0);
                for st in &statements {
                    let params: Vec<mysql_async::Value> =
                        st.params.iter().map(value::to_mysql).collect();
                    let outcome = match conn.exec_drop(st.sql.as_str(), params).await {
                        Ok(()) => Ok(conn.affected_rows()),
                        Err(e) => Err(map(&e, Some(&st.sql))),
                    };
                    match outcome {
                        Ok(n) if st.one_row && n != 1 => {
                            result = Err(Error::new(
                                ErrorKind::Guard,
                                if n == 0 {
                                    "Nothing was saved: a row you edited was changed or deleted \
                                     since it was loaded. Reload and edit again."
                                        .to_owned()
                                } else {
                                    format!(
                                        "Nothing was saved: the key matched {n} rows instead of \
                                         one, so it does not identify a single row."
                                    )
                                },
                            ));
                            break;
                        }
                        Ok(n) => {
                            if let Ok(total) = &mut result {
                                *total += n;
                            }
                        }
                        Err(e) => {
                            result = Err(e);
                            break;
                        }
                    }
                }
                let finish = match (&result, nested) {
                    (Ok(_), false) => "COMMIT".to_owned(),
                    (Ok(_), true) => format!("RELEASE SAVEPOINT {SAVEPOINT}"),
                    (Err(_), false) => "ROLLBACK".to_owned(),
                    (Err(_), true) => format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
                };
                let finished = conn.query_drop(finish).await;
                if nested && result.is_err() {
                    let _ = conn
                        .query_drop(format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                        .await;
                }
                match (finished, result) {
                    (Err(e), Ok(_)) => Err(map(&e, None)),
                    (_, r) => r,
                }
            })
        })
        .await
    }

    async fn row_key(&self, table: &TableRef) -> Result<Vec<String>> {
        let schema = table
            .schema
            .as_deref()
            .map_or(mysql_async::Value::NULL, |s| s.into());
        // Unique keys only; a functional key part has no column name and cannot identify a row
        // from the result, and a nullable column lets several rows share the key.
        let rows = self
            .meta(
                "SELECT INDEX_NAME, COLUMN_NAME, NULLABLE FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
                   AND NON_UNIQUE = 0 \
                 ORDER BY INDEX_NAME <> 'PRIMARY', INDEX_NAME, SEQ_IN_INDEX",
                vec![schema, table.table.as_str().into()],
            )
            .await?;
        let mut keys: Vec<(String, Vec<Option<String>>, bool)> = Vec::new();
        for row in &rows {
            let index = cell(row, 0).unwrap_or_default();
            let column = cell(row, 1);
            let nullable = cell(row, 2).is_some_and(|n| n.eq_ignore_ascii_case("YES"));
            match keys.last_mut() {
                Some((name, cols, any_null)) if *name == index => {
                    cols.push(column);
                    *any_null |= nullable;
                }
                _ => keys.push((index, vec![column], nullable)),
            }
        }
        Ok(keys
            .into_iter()
            .find(|(name, cols, any_null)| {
                cols.iter().all(Option::is_some) && (name == "PRIMARY" || !any_null)
            })
            .map(|(_, cols, _)| cols.into_iter().flatten().collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use treasury_core::driver::SqlDialect;

    #[test]
    fn statements_bind_values_and_review_shows_them() {
        let cs = Changeset {
            table: TableRef {
                schema: Some("shop".into()),
                table: "orders".into(),
            },
            key_columns: vec!["id".into()],
            changes: vec![
                RowChange::Update {
                    key: vec![Value::I64(7)],
                    set: vec![("note".into(), Value::Text("it's ?".into()))],
                },
                RowChange::Delete {
                    key: vec![Value::Null],
                },
            ],
        };
        let p = plan(SqlDialect::MySql, &cs);
        assert_eq!(
            p[0].sql,
            "UPDATE `shop`.`orders` SET `note` = ? WHERE `id` = ?"
        );
        assert_eq!(p[0].params.len(), 2);
        assert_eq!(
            p[0].review,
            "UPDATE `shop`.`orders` SET `note` = 'it''s ?' WHERE `id` = 7;"
        );
        assert_eq!(p[1].sql, "DELETE FROM `shop`.`orders` WHERE `id` IS NULL");
    }
}
