//! Row edits from the grid, applied in one transaction with bind parameters. When the user has
//! a transaction open (`@@TRANCOUNT` above zero), the edits join it under a savepoint.

use crate::session::{MsSqlSession, cell, int};
use crate::{error, value};
use async_trait::async_trait;
use tiberius::ToSql;
use treasury_core::driver::{Edit, SqlDialect};
use treasury_core::edit::{Changeset, RowChange, TableRef};
use treasury_core::{Error, ErrorKind, Result, Value};
use treasury_sql::render;

const D: SqlDialect = SqlDialect::MsSql;
const SAVEPOINT: &str = "treasury_edit";

struct Planned {
    sql: String,
    review: String,
    params: Vec<Value>,
    one_row: bool,
}

fn plan(changes: &Changeset) -> Vec<Planned> {
    let table = render::table(D, changes.table.schema.as_deref(), &changes.table.table);
    changes
        .changes
        .iter()
        .map(|change| {
            let mut params: Vec<Value> = Vec::new();
            let bind = |v: &Value, params: &mut Vec<Value>| {
                params.push(v.clone());
                (format!("@P{}", params.len()), render::literal(D, v))
            };
            let key = |k: &[Value], params: &mut Vec<Value>| {
                let mut sql = Vec::new();
                let mut review = Vec::new();
                for (c, v) in changes.key_columns.iter().zip(k) {
                    let col = render::ident(D, c);
                    if v.is_null() {
                        sql.push(format!("{col} IS NULL"));
                        review.push(format!("{col} IS NULL"));
                    } else {
                        let (p, l) = bind(v, params);
                        sql.push(format!("{col} = {p}"));
                        review.push(format!("{col} = {l}"));
                    }
                }
                (sql.join(" AND "), review.join(" AND "))
            };
            let (sql, review, one_row) = match change {
                RowChange::Update { key: k, set } => {
                    let mut sets = Vec::new();
                    let mut shown = Vec::new();
                    for (c, v) in set {
                        params.push(v.clone());
                        sets.push(format!("{} = @P{}", render::ident(D, c), params.len()));
                        shown.push(format!(
                            "{} = {}",
                            render::ident(D, c),
                            render::literal(D, v)
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
                    let s = format!("INSERT INTO {table} DEFAULT VALUES");
                    (s.clone(), s, false)
                }
                RowChange::Insert { values } => {
                    let cols = values
                        .iter()
                        .map(|(c, _)| render::ident(D, c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut holders = Vec::new();
                    let mut literals = Vec::new();
                    for (_, v) in values {
                        params.push(v.clone());
                        holders.push(format!("@P{}", params.len()));
                        literals.push(render::literal(D, v));
                    }
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
impl Edit for MsSqlSession {
    fn render(&self, changes: &Changeset) -> Result<Vec<String>> {
        changes.validate()?;
        Ok(plan(changes).into_iter().map(|p| p.review).collect())
    }

    async fn apply(&self, changes: &Changeset) -> Result<u64> {
        changes.validate()?;
        self.alive()?;
        let statements = plan(changes);
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .ok_or_else(|| Error::protocol("The connection is closed."))?;
        let map = |e: &tiberius::error::Error, sql: Option<&str>| {
            error::map(e, &self.host, self.port, sql)
        };
        let open = client
            .simple_query("SELECT CAST(@@TRANCOUNT AS bigint)")
            .await
            .map_err(|e| map(&e, None))?
            .into_first_result()
            .await
            .map_err(|e| map(&e, None))?;
        let nested = open.first().and_then(|r| int(r, 0)).unwrap_or(0) > 0;
        let begin = if nested {
            format!("SAVE TRANSACTION {SAVEPOINT}")
        } else {
            "BEGIN TRANSACTION".to_owned()
        };
        MsSqlSession::batch(client, &begin)
            .await
            .map_err(|e| map(&e, None))?;
        let mut result: Result<u64> = Ok(0);
        for st in &statements {
            let params: Vec<value::Param> = st.params.iter().map(value::to_param).collect();
            let refs: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
            let n = match client.execute(st.sql.as_str(), &refs).await {
                Ok(r) => r.rows_affected().iter().sum::<u64>(),
                Err(e) => {
                    result = Err(map(&e, Some(&st.sql)));
                    break;
                }
            };
            if st.one_row && n != 1 {
                result = Err(Error::new(
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
                break;
            }
            if let Ok(total) = &mut result {
                *total += n;
            }
        }
        let finish = match (&result, nested) {
            (Ok(_), false) => "COMMIT TRANSACTION".to_owned(),
            // A savepoint needs no release; the user's own COMMIT keeps the edits.
            (Ok(_), true) => return result,
            (Err(_), false) => "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION".to_owned(),
            (Err(_), true) => format!("ROLLBACK TRANSACTION {SAVEPOINT}"),
        };
        match (MsSqlSession::batch(client, &finish).await, result) {
            (Err(e), Ok(_)) => Err(map(&e, None)),
            (_, r) => r,
        }
    }

    async fn row_key(&self, table: &TableRef) -> Result<Vec<String>> {
        // OBJECT_ID resolves an unqualified name through the user's default schema. Filtered
        // indexes cover only some rows, and a nullable key column lets rows share the key.
        let name = render::table(D, table.schema.as_deref(), &table.table);
        let rows = self
            .meta(
                "SELECT i.name, CAST(i.is_primary_key AS int), c.name, CAST(c.is_nullable AS int) \
                 FROM sys.indexes i \
                 JOIN sys.index_columns ic \
                   ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = OBJECT_ID(@P1) AND i.is_unique = 1 \
                   AND i.has_filter = 0 AND ic.is_included_column = 0 \
                 ORDER BY i.is_primary_key DESC, i.index_id, ic.key_ordinal",
                &[&name.as_str()],
            )
            .await?;
        let mut keys: Vec<(String, bool, Vec<String>, bool)> = Vec::new();
        for row in &rows {
            let index = cell(row, 0).unwrap_or_default();
            let primary = int(row, 1) == Some(1);
            let column = cell(row, 2).unwrap_or_default();
            let nullable = int(row, 3) == Some(1);
            match keys.last_mut() {
                Some((name, _, cols, any_null)) if *name == index => {
                    cols.push(column);
                    *any_null |= nullable;
                }
                _ => keys.push((index, primary, vec![column], nullable)),
            }
        }
        Ok(keys
            .into_iter()
            .find(|(_, primary, _, any_null)| *primary || !any_null)
            .map(|(_, _, cols, _)| cols)
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_bind_values() {
        let cs = Changeset {
            table: TableRef {
                schema: Some("dbo".into()),
                table: "orders".into(),
            },
            key_columns: vec!["id".into()],
            changes: vec![RowChange::Update {
                key: vec![Value::I64(7)],
                set: vec![("note".into(), Value::Text("it's".into()))],
            }],
        };
        let p = plan(&cs);
        assert_eq!(
            p[0].sql,
            "UPDATE [dbo].[orders] SET [note] = @P1 WHERE [id] = @P2"
        );
        assert_eq!(
            p[0].review,
            "UPDATE [dbo].[orders] SET [note] = N'it''s' WHERE [id] = 7;"
        );
    }
}
