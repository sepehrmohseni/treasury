//! Which table a SELECT reads, so the grid can edit its rows in place.
//!
//! Only a plain SELECT from one table qualifies: no joins, grouping, DISTINCT, set operations or
//! WITH, because then every result row is one table row and every plain column is that row's
//! column. Anything computed stays visible but cannot be edited.

use crate::dialect::{Rules, parser_dialect};
use sqlparser::ast::{
    Expr, GroupByExpr, Ident, ObjectName, ObjectNamePart, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor, WildcardAdditionalOptions,
};
use sqlparser::parser::Parser;
use treasury_core::analyze::{EditSource, Projected};
use treasury_core::driver::SqlDialect;
use treasury_core::edit::TableRef;

const ONE_TABLE: &str = "Only the result of a SELECT from one table, without joins, grouping, \
                         DISTINCT or WITH, can be edited.";

/// The table behind `statement`'s result, or why its rows cannot be edited.
pub fn source(rules: &Rules, statement: &str) -> Result<EditSource, String> {
    if rules.executable_comments && statement.contains("/*!") {
        return Err("Statements with MySQL executable comments cannot be edited.".into());
    }
    let dialect = parser_dialect(rules.dialect);
    let parsed = Parser::parse_sql(&*dialect, statement)
        .map_err(|_| "Treasury could not read this statement, so its rows cannot be edited.")?;
    let [Statement::Query(query)] = parsed.as_slice() else {
        return Err(ONE_TABLE.into());
    };
    if query.with.is_some() {
        return Err(ONE_TABLE.into());
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(ONE_TABLE.into());
    };
    let grouped = match &select.group_by {
        GroupByExpr::Expressions(exprs, modifiers) => !exprs.is_empty() || !modifiers.is_empty(),
        GroupByExpr::All(_) => true,
    };
    if select.distinct.is_some()
        || select.into.is_some()
        || select.having.is_some()
        || grouped
        || !select.lateral_views.is_empty()
        || select.qualify.is_some()
        || !select.named_window.is_empty()
    {
        return Err(ONE_TABLE.into());
    }
    let [from] = select.from.as_slice() else {
        return Err(ONE_TABLE.into());
    };
    if !from.joins.is_empty() {
        return Err(ONE_TABLE.into());
    }
    let TableFactor::Table {
        name,
        alias,
        args: None,
        ..
    } = &from.relation
    else {
        return Err(ONE_TABLE.into());
    };
    let parts = idents(name).ok_or(ONE_TABLE)?;
    let norm = |i: &Ident| fold(rules.dialect, i);
    let (schema, table) = match parts.as_slice() {
        [t] => (None, norm(t)),
        [s, t] => (Some(norm(s)), norm(t)),
        _ => {
            return Err("Rows of a table in another database cannot be edited here.".into());
        }
    };
    // What a qualified column may be qualified with: the alias, or else the table's name.
    let qualifier = match alias {
        Some(a) => vec![norm(&a.name)],
        None => parts.iter().map(|i| norm(i)).collect(),
    };
    let qualified_by_table = |q: &[&Ident]| {
        let q: Vec<String> = q.iter().map(|i| norm(i)).collect();
        !q.is_empty() && qualifier.ends_with(&q)
    };
    let column_of = |e: &Expr| -> Option<String> {
        match e {
            Expr::Identifier(i) => Some(norm(i)),
            Expr::CompoundIdentifier(p) => match p.split_last() {
                Some((col, q)) if qualified_by_table(&q.iter().collect::<Vec<_>>()) => {
                    Some(norm(col))
                }
                _ => None,
            },
            _ => None,
        }
    };
    let mut items = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        items.push(match item {
            SelectItem::Wildcard(opts) => star(opts)?,
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(q), opts) => {
                let q = idents(q).ok_or(ONE_TABLE)?;
                if !qualified_by_table(&q) {
                    return Err(ONE_TABLE.into());
                }
                star(opts)?
            }
            SelectItem::QualifiedWildcard(..) => Projected::Expression,
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                column_of(e).map_or(Projected::Expression, Projected::Column)
            }
            // `expr AS (a, b)` fills a number of columns only the server knows.
            SelectItem::ExprWithAliases { .. } => return Err(ONE_TABLE.into()),
        });
    }
    if items.iter().all(|i| *i == Projected::Expression) {
        return Err("Every column of this result is computed, so there is nothing to edit.".into());
    }
    Ok(EditSource {
        table: TableRef { schema, table },
        items,
    })
}

/// `*`, unless it renames or replaces columns: then a result column's name no longer says which
/// table column it holds.
fn star(opts: &WildcardAdditionalOptions) -> Result<Projected, String> {
    if opts.opt_replace.is_some() || opts.opt_rename.is_some() {
        return Err("A * that renames or replaces columns cannot be edited.".into());
    }
    Ok(Projected::AllColumns)
}

fn idents(name: &ObjectName) -> Option<Vec<&Ident>> {
    name.0.iter().map(ObjectNamePart::as_ident).collect()
}

/// An identifier as the server stores it: PostgreSQL folds unquoted names to lower case,
/// Snowflake, Oracle and Db2 to upper case; MySQL and SQL Server keep them as written.
fn fold(dialect: SqlDialect, i: &Ident) -> String {
    if i.quote_style.is_some() {
        return i.value.clone();
    }
    match dialect {
        SqlDialect::Postgres => i.value.to_lowercase(),
        SqlDialect::Snowflake | SqlDialect::Oracle | SqlDialect::Db2 => i.value.to_uppercase(),
        _ => i.value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::rules;
    use Projected::{AllColumns, Column, Expression};

    fn ok(d: SqlDialect, sql: &str) -> EditSource {
        source(&rules(d), sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn col(c: &str) -> Projected {
        Column(c.into())
    }

    #[test]
    fn plain_selects_name_their_table_and_columns() {
        let s = ok(
            SqlDialect::Postgres,
            "SELECT id, Name, \"Mixed\", o.total AS t, upper(note), * FROM Shop.Orders o \
             WHERE id > 3 ORDER BY id LIMIT 100",
        );
        assert_eq!(
            s.table,
            TableRef {
                schema: Some("shop".into()),
                table: "orders".into()
            }
        );
        assert_eq!(
            s.items,
            vec![
                col("id"),
                col("name"),
                col("Mixed"),
                col("total"),
                Expression,
                AllColumns
            ]
        );
        let s = ok(
            SqlDialect::MySql,
            "SELECT `Id`, orders.note FROM shop.orders",
        );
        assert_eq!(s.table.schema.as_deref(), Some("shop"));
        assert_eq!(s.items, vec![col("Id"), col("note")]);
        let s = ok(SqlDialect::MsSql, "SELECT TOP 10 [id], dbo.t.* FROM dbo.t");
        assert_eq!(s.items, vec![col("id"), AllColumns]);
    }

    #[test]
    fn a_column_qualified_by_another_name_is_not_the_tables() {
        let s = ok(SqlDialect::Postgres, "SELECT x.id, t.id FROM t");
        assert_eq!(s.items, vec![Expression, col("id")]);
    }

    #[test]
    fn anything_but_one_table_cannot_be_edited() {
        for sql in [
            "SELECT a.id FROM a JOIN b ON a.id = b.id",
            "SELECT id FROM a, b",
            "SELECT DISTINCT id FROM a",
            "SELECT id, count(*) FROM a GROUP BY id",
            "SELECT id FROM a UNION SELECT id FROM b",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT * FROM (SELECT id FROM a) s",
            "SELECT * FROM generate_series(1, 3)",
            "SELECT 1",
            "SELECT count(*) FROM a",
            "SELECT * INTO b FROM a",
            "UPDATE a SET x = 1",
            "SELECT 1; SELECT 2",
            "SELECT * FROM other.db.t",
        ] {
            assert!(
                source(&rules(SqlDialect::Postgres), sql).is_err(),
                "{sql} was editable"
            );
        }
        assert!(source(&rules(SqlDialect::MySql), "SELECT /*! 1, */ id FROM t").is_err());
    }
}
