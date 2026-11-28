//! Narrowing what a table's data page reads: the condition typed in its filter box becomes the
//! WHERE clause of the page's SELECT, and the columns clicked in the grid its ORDER BY.
//!
//! The condition is parsed on its own as one expression, so it cannot end the statement or add
//! clauses of its own. The clauses are then added to the text, and the result is parsed again:
//! it is used only if its AST is the page's SELECT with exactly that WHERE and ORDER BY. The
//! condition is kept as typed, so a server error points at the text the user wrote.

use crate::dialect::parser_dialect;
use crate::render;
use sqlparser::ast::{Expr, OrderByKind, OrderBySort, SetExpr, Statement, visit_statements};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;
use std::ops::ControlFlow;
use treasury_core::driver::SqlDialect;

/// A column the rows are sorted by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sort {
    /// The column's name, as the result names it.
    pub column: String,
    /// Largest first.
    pub descending: bool,
}

/// The order after a click on `column`'s header. A click sorts by that column alone, smallest
/// first, then largest first, then not at all. With `add` (a shift-click) the other columns
/// stay, and the column is added after them or turned around where it is.
pub fn cycled(order: &[Sort], column: &str, add: bool) -> Vec<Sort> {
    let now = order
        .iter()
        .find(|s| s.column == column)
        .map(|s| s.descending);
    let mut out: Vec<Sort> = if add {
        order.to_vec()
    } else {
        order
            .iter()
            .filter(|s| s.column == column)
            .cloned()
            .collect()
    };
    match now {
        None => out.push(Sort {
            column: column.to_owned(),
            descending: false,
        }),
        Some(false) => {
            for s in out.iter_mut().filter(|s| s.column == column) {
                s.descending = true;
            }
        }
        Some(true) => out.retain(|s| s.column != column),
    }
    out
}

/// `base`, a plain `SELECT ... FROM table`, reading only the rows that match `filter` (a
/// condition such as `id > 10`; empty for every row), in `order`.
pub fn narrow(
    dialect: SqlDialect,
    base: &str,
    filter: &str,
    order: &[Sort],
) -> Result<String, String> {
    let parser = parser_dialect(dialect);
    let unreadable = || "Treasury could not add the filter or the sort to this table's query.";
    let parsed = Parser::parse_sql(&*parser, base).map_err(|_| unreadable())?;
    let [Statement::Query(original)] = parsed.as_slice() else {
        return Err(unreadable().into());
    };
    match original.body.as_ref() {
        SetExpr::Select(s) if s.selection.is_none() && original.order_by.is_none() => {}
        _ => return Err(unreadable().into()),
    }
    let filter = without_where(filter.trim());
    let condition = if filter.is_empty() {
        None
    } else {
        Some(condition(dialect, filter)?)
    };
    let mut text = base.trim_end().to_owned();
    if condition.is_some() {
        // On a line of its own, so a comment at the end of the condition ends there.
        text.push_str("\nWHERE ");
        text.push_str(filter);
    }
    if !order.is_empty() {
        let columns: Vec<String> = order
            .iter()
            .map(|s| {
                let name = render::ident(dialect, &s.column);
                if s.descending {
                    format!("{name} DESC")
                } else {
                    name
                }
            })
            .collect();
        text.push_str("\nORDER BY ");
        text.push_str(&columns.join(", "));
    }
    let again = Parser::parse_sql(&*parser, &text).map_err(|_| unreadable())?;
    let [Statement::Query(narrowed)] = again.as_slice() else {
        return Err(unreadable().into());
    };
    let mut rest = narrowed.as_ref().clone();
    let SetExpr::Select(select) = rest.body.as_mut() else {
        return Err(unreadable().into());
    };
    if select.selection.take() != condition || !ordered_by(rest.order_by.take(), order) {
        return Err(unreadable().into());
    }
    if rest != **original {
        return Err(unreadable().into());
    }
    Ok(text)
}

/// `filter` without a `WHERE` typed before it.
fn without_where(filter: &str) -> &str {
    match filter.get(..5) {
        Some(w)
            if w.eq_ignore_ascii_case("where")
                && filter[5..].starts_with(|c: char| c.is_whitespace()) =>
        {
            filter[5..].trim_start()
        }
        _ => filter,
    }
}

/// `filter` read as one condition that only reads.
fn condition(dialect: SqlDialect, filter: &str) -> Result<Expr, String> {
    let parser = parser_dialect(dialect);
    let mut p = Parser::new(&*parser)
        .try_with_sql(filter)
        .map_err(|e| format!("The filter cannot be read: {}.", plain(&e.to_string())))?;
    let expr = p
        .parse_expr()
        .map_err(|e| format!("The filter cannot be read: {}.", plain(&e.to_string())))?;
    if p.peek_token().token != Token::EOF {
        return Err(
            "The filter must be one condition, such as id > 10, with nothing after it.".into(),
        );
    }
    let mut writes = false;
    let _ = visit_statements(&expr, |s| {
        if !matches!(s, Statement::Query(_)) {
            writes = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    if writes {
        return Err("The filter may only read: it changes data.".into());
    }
    Ok(expr)
}

/// A parser error without the parser's own prefix.
fn plain(error: &str) -> String {
    error
        .strip_prefix("sql parser error: ")
        .unwrap_or(error)
        .trim_end_matches('.')
        .to_owned()
}

/// Whether `got` sorts by exactly the columns of `order`, each its way.
fn ordered_by(got: Option<sqlparser::ast::OrderBy>, order: &[Sort]) -> bool {
    let Some(got) = got else {
        return order.is_empty();
    };
    if got.interpolate.is_some() {
        return false;
    }
    let OrderByKind::Expressions(exprs) = got.kind else {
        return false;
    };
    exprs.len() == order.len()
        && exprs.iter().zip(order).all(|(e, s)| {
            matches!(&e.expr, Expr::Identifier(i) if i.value == s.column && i.quote_style.is_some())
                && e.options.sort == s.descending.then_some(OrderBySort::Desc)
                && e.options.nulls_first.is_none()
                && e.with_fill.is_none()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use treasury_core::driver::SqlDialect as D;

    fn asc(c: &str) -> Sort {
        Sort {
            column: c.into(),
            descending: false,
        }
    }

    fn desc(c: &str) -> Sort {
        Sort {
            column: c.into(),
            descending: true,
        }
    }

    #[test]
    fn a_filter_and_a_sort_become_where_and_order_by() {
        assert_eq!(
            narrow(
                D::Postgres,
                "SELECT * FROM \"shop\".\"orders\"",
                "total > 100 and status = 'paid'",
                &[desc("placed_at"), asc("id")]
            )
            .unwrap(),
            "SELECT * FROM \"shop\".\"orders\"\nWHERE total > 100 and status = 'paid'\n\
             ORDER BY \"placed_at\" DESC, \"id\""
        );
        assert_eq!(
            narrow(D::MySql, "SELECT * FROM `db`.`t`", "", &[asc("Name")]).unwrap(),
            "SELECT * FROM `db`.`t`\nORDER BY `Name`"
        );
        assert_eq!(
            narrow(
                D::MsSql,
                "SELECT * FROM [dbo].[t]",
                "WHERE a in (1, 2)",
                &[]
            )
            .unwrap(),
            "SELECT * FROM [dbo].[t]\nWHERE a in (1, 2)"
        );
        assert_eq!(
            narrow(D::Postgres, "SELECT * FROM t", "  ", &[]).unwrap(),
            "SELECT * FROM t"
        );
    }

    #[test]
    fn odd_column_names_are_quoted() {
        assert_eq!(
            narrow(D::Postgres, "SELECT * FROM t", "", &[asc("we\"ird name")]).unwrap(),
            "SELECT * FROM t\nORDER BY \"we\"\"ird name\""
        );
        assert_eq!(
            narrow(D::MsSql, "SELECT * FROM t", "", &[desc("a]b")]).unwrap(),
            "SELECT * FROM t\nORDER BY [a]]b] DESC"
        );
    }

    #[test]
    fn a_comment_ends_with_the_condition() {
        assert_eq!(
            narrow(
                D::Postgres,
                "SELECT * FROM t",
                "a = 1 -- the first",
                &[asc("b")]
            )
            .unwrap(),
            "SELECT * FROM t\nWHERE a = 1 -- the first\nORDER BY \"b\""
        );
    }

    #[test]
    fn a_filter_is_one_condition_and_nothing_else() {
        for (d, f) in [
            (D::Postgres, "1 = 1; drop table t"),
            (D::Postgres, "a = 1 order by b"),
            (D::Postgres, "a = 1 limit 5"),
            (D::Postgres, "a = 1) union (select * from other"),
            (D::MySql, "1 = 1; delete from t"),
            (D::MsSql, "1 = 1 drop table t"),
            (D::Postgres, "a ="),
            (D::Postgres, "'unclosed"),
            (D::Postgres, "a = 1 /* unclosed"),
        ] {
            assert!(narrow(d, "SELECT * FROM t", f, &[]).is_err(), "{d:?} {f}");
        }
    }

    #[test]
    fn subqueries_may_read_but_not_write() {
        assert!(
            narrow(
                D::Postgres,
                "SELECT * FROM t",
                "id in (select order_id from items where qty > 2)",
                &[]
            )
            .is_ok()
        );
        assert!(
            narrow(
                D::Postgres,
                "SELECT * FROM t",
                "id in (with d as (delete from items returning order_id) select order_id from d)",
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn only_a_plain_select_is_narrowed() {
        for base in [
            "SELECT * FROM t WHERE a = 1",
            "SELECT * FROM t ORDER BY a",
            "SELECT 1 UNION SELECT 2",
            "DELETE FROM t",
            "not sql",
        ] {
            assert!(narrow(D::Postgres, base, "a = 2", &[]).is_err(), "{base}");
        }
    }

    #[test]
    fn the_row_limit_still_applies_after_narrowing() {
        let rules = crate::dialect::rules(D::Postgres);
        let text = narrow(D::Postgres, "SELECT * FROM t", "a > 1", &[desc("a")]).unwrap();
        assert!(matches!(
            crate::limit::plan(&rules, &text, 1000),
            treasury_core::analyze::LimitPlan::Rewritten { text, .. }
                if text.ends_with("ORDER BY \"a\" DESC LIMIT 1000")
        ));
        let rules = crate::dialect::rules(D::MsSql);
        let text = narrow(D::MsSql, "SELECT * FROM [t]", "a > 1", &[asc("a")]).unwrap();
        assert!(matches!(
            crate::limit::plan(&rules, &text, 1000),
            treasury_core::analyze::LimitPlan::Rewritten { text, .. }
                if text.starts_with("SELECT TOP 1000 * FROM [t]")
        ));
    }

    #[test]
    fn a_click_sorts_up_then_down_then_not_at_all() {
        let one = cycled(&[], "a", false);
        assert_eq!(one, vec![asc("a")]);
        let two = cycled(&one, "a", false);
        assert_eq!(two, vec![desc("a")]);
        assert_eq!(cycled(&two, "a", false), vec![]);
        // Another column replaces it, unless shift is held.
        assert_eq!(cycled(&two, "b", false), vec![asc("b")]);
        assert_eq!(cycled(&two, "b", true), vec![desc("a"), asc("b")]);
        assert_eq!(
            cycled(&[desc("a"), asc("b")], "b", true),
            vec![desc("a"), desc("b")]
        );
        assert_eq!(cycled(&[desc("a"), desc("b")], "a", true), vec![desc("b")]);
    }
}
