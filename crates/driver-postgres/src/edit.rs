//! Row edits from the grid, applied in one transaction with bind parameters.
//!
//! When the user already has a transaction open, the edits join it under a savepoint instead of
//! starting (and committing) one of their own, so they are committed or rolled back with the
//! rest of that transaction.

use crate::binary::Param;
use crate::session::PgSession;
use async_trait::async_trait;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use treasury_core::driver::{Edit, SqlDialect};
use treasury_core::edit::{Changeset, RowChange, TableRef};
use treasury_core::{Error, ErrorKind, Result, Value};
use treasury_sql::render;

const D: SqlDialect = SqlDialect::Postgres;
const SAVEPOINT: &str = "treasury_edit";

/// One statement: its text with `$n` placeholders, the values bound to them, and the same
/// statement with the values written in for review.
struct Planned {
    sql: String,
    review: String,
    params: Vec<Value>,
    /// UPDATE and DELETE must touch exactly one row.
    one_row: bool,
}

/// Collects bind values; each call gives the placeholder and the literal for review.
#[derive(Default)]
struct Binds(Vec<Value>);

impl Binds {
    fn bind(&mut self, v: &Value) -> (String, String) {
        self.0.push(v.clone());
        (format!("${}", self.0.len()), render::literal(D, v))
    }
}

/// `"id" = $1 AND "region" IS NULL`, and its review form.
fn key_clause(columns: &[String], key: &[Value], binds: &mut Binds) -> (String, String) {
    let mut sql = Vec::new();
    let mut review = Vec::new();
    for (c, v) in columns.iter().zip(key) {
        let col = render::ident(D, c);
        if v.is_null() {
            sql.push(format!("{col} IS NULL"));
            review.push(format!("{col} IS NULL"));
        } else {
            let (p, l) = binds.bind(v);
            sql.push(format!("{col} = {p}"));
            review.push(format!("{col} = {l}"));
        }
    }
    (sql.join(" AND "), review.join(" AND "))
}

fn plan(changes: &Changeset) -> Vec<Planned> {
    let table = render::table(D, changes.table.schema.as_deref(), &changes.table.table);
    changes
        .changes
        .iter()
        .map(|change| {
            let mut binds = Binds::default();
            let (sql, review, one_row) = match change {
                RowChange::Update { key, set } => {
                    let mut sets = Vec::new();
                    let mut shown = Vec::new();
                    for (c, v) in set {
                        let col = render::ident(D, c);
                        let (p, l) = binds.bind(v);
                        sets.push(format!("{col} = {p}"));
                        shown.push(format!("{col} = {l}"));
                    }
                    let (filter, filter_shown) = key_clause(&changes.key_columns, key, &mut binds);
                    (
                        format!("UPDATE {table} SET {} WHERE {filter}", sets.join(", ")),
                        format!(
                            "UPDATE {table} SET {} WHERE {filter_shown}",
                            shown.join(", ")
                        ),
                        true,
                    )
                }
                RowChange::Insert { values } if values.is_empty() => {
                    let sql = format!("INSERT INTO {table} DEFAULT VALUES");
                    (sql.clone(), sql, false)
                }
                RowChange::Insert { values } => {
                    let cols = values
                        .iter()
                        .map(|(c, _)| render::ident(D, c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let (holders, literals): (Vec<_>, Vec<_>) =
                        values.iter().map(|(_, v)| binds.bind(v)).unzip();
                    (
                        format!(
                            "INSERT INTO {table} ({cols}) VALUES ({})",
                            holders.join(", ")
                        ),
                        format!(
                            "INSERT INTO {table} ({cols}) VALUES ({})",
                            literals.join(", ")
                        ),
                        false,
                    )
                }
                RowChange::Delete { key } => {
                    let (filter, filter_shown) = key_clause(&changes.key_columns, key, &mut binds);
                    (
                        format!("DELETE FROM {table} WHERE {filter}"),
                        format!("DELETE FROM {table} WHERE {filter_shown}"),
                        true,
                    )
                }
            };
            Planned {
                sql,
                review: format!("{review};"),
                params: binds.0,
                one_row,
            }
        })
        .collect()
}

impl PgSession {
    async fn run_all(&self, statements: &[Planned]) -> Result<u64> {
        let mut total = 0;
        for st in statements {
            let params: Vec<Param> = st.params.iter().cloned().map(Param).collect();
            let refs: Vec<&(dyn ToSql + Sync)> =
                params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
            let n = self
                .client
                .execute(st.sql.as_str(), &refs)
                .await
                .map_err(|e| self.map(&e))?;
            if st.one_row && n != 1 {
                return Err(Error::new(
                    ErrorKind::Guard,
                    if n == 0 {
                        "Nothing was saved: a row you edited was changed or deleted since it was \
                         loaded. Reload and edit again."
                            .to_owned()
                    } else {
                        format!(
                            "Nothing was saved: the key matched {n} rows instead of one, so it \
                             does not identify a single row."
                        )
                    },
                ));
            }
            total += n;
        }
        Ok(total)
    }
}

#[async_trait]
impl Edit for PgSession {
    fn render(&self, changes: &Changeset) -> Result<Vec<String>> {
        changes.validate()?;
        Ok(plan(changes).into_iter().map(|p| p.review).collect())
    }

    async fn apply(&self, changes: &Changeset) -> Result<u64> {
        changes.validate()?;
        self.alive()?;
        let statements = plan(changes);
        // A savepoint only works inside a transaction block, which tells us whether one is open.
        let nested = match self
            .client
            .batch_execute(&format!("SAVEPOINT {SAVEPOINT}"))
            .await
        {
            Ok(()) => true,
            Err(e) if e.code() == Some(&SqlState::NO_ACTIVE_SQL_TRANSACTION) => {
                self.client
                    .batch_execute("BEGIN")
                    .await
                    .map_err(|e| self.map(&e))?;
                false
            }
            Err(e) => return Err(self.map(&e)),
        };
        let result = self.run_all(&statements).await;
        let finish = match (&result, nested) {
            (Ok(_), false) => "COMMIT".to_owned(),
            (Ok(_), true) => format!("RELEASE SAVEPOINT {SAVEPOINT}"),
            (Err(_), false) => "ROLLBACK".to_owned(),
            (Err(_), true) => {
                format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}; RELEASE SAVEPOINT {SAVEPOINT}")
            }
        };
        match (self.client.batch_execute(&finish).await, &result) {
            // A failed COMMIT (a deferred constraint, say) means nothing was saved.
            (Err(e), Ok(_)) => Err(self.map(&e)),
            _ => result,
        }
    }

    async fn row_key(&self, table: &TableRef) -> Result<Vec<String>> {
        self.alive()?;
        let name = render::table(D, table.schema.as_deref(), &table.table);
        // to_regclass resolves an unqualified name through the session's search_path. Partial
        // and expression indexes cannot identify a row; a unique key with a nullable column
        // can match several rows that are all NULL there.
        let rows = self
            .client
            .query(
                "SELECT i.indisprimary, bool_and(a.attnotnull), \
                        array_agg(a.attname::text ORDER BY k.ord) \
                 FROM pg_index i \
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                 WHERE i.indrelid = to_regclass($1) AND i.indisunique \
                   AND i.indpred IS NULL AND i.indexprs IS NULL \
                 GROUP BY i.indexrelid, i.indisprimary \
                 ORDER BY i.indisprimary DESC, count(*), i.indexrelid",
                &[&name],
            )
            .await
            .map_err(|e| self.map(&e))?;
        Ok(rows
            .iter()
            .find(|r| r.get::<_, bool>(0) || r.get::<_, bool>(1))
            .map(|r| r.get::<_, Vec<String>>(2))
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(list: Vec<RowChange>) -> Changeset {
        Changeset {
            table: TableRef {
                schema: Some("public".into()),
                table: "orders".into(),
            },
            key_columns: vec!["id".into(), "region".into()],
            changes: list,
            expire: None,
        }
    }

    #[test]
    fn statements_bind_every_value() {
        let cs = changes(vec![
            RowChange::Update {
                key: vec![Value::I64(7), Value::Text("eu".into())],
                set: vec![("status".into(), Value::Text("paid".into()))],
            },
            RowChange::Delete {
                key: vec![Value::I64(8), Value::Null],
            },
            RowChange::Insert {
                values: vec![
                    ("id".into(), Value::I64(9)),
                    ("note".into(), Value::Text("it's".into())),
                ],
            },
        ]);
        let p = plan(&cs);
        assert_eq!(
            p[0].sql,
            "UPDATE \"public\".\"orders\" SET \"status\" = $1 WHERE \"id\" = $2 AND \"region\" = $3"
        );
        assert_eq!(p[0].params.len(), 3);
        assert!(p[0].one_row);
        assert_eq!(
            p[1].sql,
            "DELETE FROM \"public\".\"orders\" WHERE \"id\" = $1 AND \"region\" IS NULL"
        );
        assert_eq!(
            p[2].sql,
            "INSERT INTO \"public\".\"orders\" (\"id\", \"note\") VALUES ($1, $2)"
        );
        assert!(!p[2].one_row);
    }

    #[test]
    fn review_text_has_literals() {
        let cs = changes(vec![RowChange::Insert {
            values: (1..=11)
                .map(|c| (format!("c{c}"), Value::I64(100 + c)))
                .collect(),
        }]);
        let text = plan(&cs)[0].review.clone();
        assert!(text.ends_with("VALUES (101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111);"));
        // A value that looks like a placeholder stays a value.
        let cs = changes(vec![RowChange::Insert {
            values: vec![
                ("a$1".into(), Value::Text("$2".into())),
                ("b".into(), Value::I64(5)),
            ],
        }]);
        assert_eq!(
            plan(&cs)[0].review,
            "INSERT INTO \"public\".\"orders\" (\"a$1\", \"b\") VALUES ('$2', 5);"
        );
        let cs = changes(vec![RowChange::Update {
            key: vec![Value::I64(1), Value::Text("eu".into())],
            set: vec![("note".into(), Value::Text("it's".into()))],
        }]);
        assert_eq!(
            plan(&cs)[0].review,
            "UPDATE \"public\".\"orders\" SET \"note\" = 'it''s' WHERE \"id\" = 1 AND \"region\" = 'eu';"
        );
    }
}
