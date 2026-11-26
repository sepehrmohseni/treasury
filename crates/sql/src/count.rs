//! Deriving `SELECT COUNT(*)` for an UPDATE or DELETE, so the user sees how many rows it would
//! change before it runs. The preview never executes the statement: triggers, sequences,
//! non-transactional tables and implicit commits make "run it and roll back" unsafe.

use crate::dialect::{Rules, parser_dialect};
use sqlparser::ast::{FromTable, SetExpr, Statement, TableWithJoins, UpdateTableFromKind};
use sqlparser::parser::Parser;
use treasury_core::analyze::CountPlan;

/// How to count the rows `statement` would change.
pub fn plan(rules: &Rules, statement: &str) -> CountPlan {
    if rules.executable_comments && statement.contains("/*!") {
        return underivable("it contains MySQL executable comments");
    }
    let dialect = parser_dialect(rules.dialect);
    let Ok(parsed) = Parser::parse_sql(&*dialect, statement) else {
        return underivable("Treasury could not parse it");
    };
    let [st] = parsed.as_slice() else {
        return underivable("it holds more than one statement");
    };
    let (tables, selection) = match st {
        Statement::Update(u) => {
            if !u.order_by.is_empty() || u.limit.is_some() {
                return underivable("it has ORDER BY or LIMIT");
            }
            let mut tables = vec![u.table.clone()];
            match &u.from {
                Some(UpdateTableFromKind::BeforeSet(t) | UpdateTableFromKind::AfterSet(t)) => {
                    tables.extend(t.iter().cloned());
                }
                None => {}
            }
            (tables, u.selection.as_ref())
        }
        Statement::Delete(d) => {
            if !d.order_by.is_empty() || d.limit.is_some() {
                return underivable("it has ORDER BY or LIMIT");
            }
            let mut tables: Vec<TableWithJoins> = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t.clone(),
            };
            if let Some(using) = &d.using {
                tables.extend(using.iter().cloned());
            }
            if tables.is_empty() {
                return underivable("it names no table to count");
            }
            (tables, d.selection.as_ref())
        }
        Statement::Merge(_) => return underivable("MERGE statements cannot be previewed"),
        Statement::Query(q)
            if matches!(
                q.body.as_ref(),
                SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Insert(_) | SetExpr::Merge(_)
            ) =>
        {
            return underivable("it changes data inside a WITH clause");
        }
        _ => return CountPlan::NotApplicable,
    };
    let from = tables
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let query = match selection {
        Some(w) => format!("SELECT COUNT(*) FROM {from} WHERE {w}"),
        None => format!("SELECT COUNT(*) FROM {from}"),
    };
    // The pieces come from the AST, so they print as valid SQL; check anyway.
    match Parser::parse_sql(&*dialect, &query) {
        Ok(p) if matches!(p.as_slice(), [Statement::Query(_)]) => CountPlan::Query(query),
        _ => underivable("its WHERE clause could not be turned into a count"),
    }
}

fn underivable(why: &str) -> CountPlan {
    CountPlan::Underivable {
        reason: format!("Treasury cannot preview how many rows it changes: {why}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::rules;
    use treasury_core::driver::SqlDialect as D;

    fn q(d: D, s: &str) -> String {
        match plan(&rules(d), s) {
            CountPlan::Query(q) => q,
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn same_tables_and_where() {
        assert_eq!(
            q(D::Postgres, "update orders set status = 'x' where id < 10"),
            "SELECT COUNT(*) FROM orders WHERE id < 10"
        );
        assert_eq!(
            q(D::Postgres, "delete from orders"),
            "SELECT COUNT(*) FROM orders"
        );
        assert_eq!(
            q(
                D::Postgres,
                "update a set x = b.x from b where a.id = b.id and b.flag"
            ),
            "SELECT COUNT(*) FROM a, b WHERE a.id = b.id AND b.flag"
        );
        assert_eq!(
            q(D::Postgres, "delete from a using b where a.id = b.id"),
            "SELECT COUNT(*) FROM a, b WHERE a.id = b.id"
        );
        assert_eq!(
            q(
                D::MySql,
                "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.a = 1 WHERE t2.b = 2"
            ),
            "SELECT COUNT(*) FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.b = 2"
        );
        assert_eq!(
            q(D::MsSql, "DELETE FROM [dbo].[t] WHERE [a] = N'x'"),
            "SELECT COUNT(*) FROM [dbo].[t] WHERE [a] = N'x'"
        );
    }

    #[test]
    fn forms_that_cannot_be_previewed() {
        for (d, s) in [
            (D::MySql, "delete from t order by id limit 10"),
            (D::MySql, "update t set a = 1 limit 5"),
            (
                D::Postgres,
                "merge into t using s on t.id = s.id when matched then delete",
            ),
            (D::Postgres, "with x as (select 1) delete from t"),
            (D::Postgres, "update t set a = 1; delete from t"),
            (D::Postgres, "upd ate t"),
        ] {
            assert!(
                matches!(plan(&rules(d), s), CountPlan::Underivable { .. }),
                "{s}"
            );
        }
        assert_eq!(
            plan(&rules(D::Postgres), "insert into t values (1)"),
            CountPlan::NotApplicable
        );
        assert_eq!(
            plan(&rules(D::Postgres), "select 1"),
            CountPlan::NotApplicable
        );
    }
}
