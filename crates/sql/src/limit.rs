//! Adding the default row limit to a simple SELECT.
//!
//! The decision is made on the AST: only a top-level SELECT without a limit, without locking
//! clauses and without data-changing parts qualifies. The limit clause is then inserted into the
//! original text at the position the dialect's grammar requires, and the result is parsed again:
//! it is used only if its AST equals the original AST with the limit added. Keeping the user's
//! text (instead of printing the AST) keeps comments and formatting, and lets server error
//! positions be mapped back exactly.

use crate::dialect::{LimitStyle, Rules, parser_dialect};
use crate::tokens::{is_blank, tokenize};
use sqlparser::ast::{Expr, LimitClause, Query, SetExpr, Statement, TopQuantity, visit_statements};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;
use std::ops::ControlFlow;
use treasury_core::analyze::LimitPlan;

/// How to cap `statement` at `limit` rows.
pub fn plan(rules: &Rules, statement: &str, limit: u64) -> LimitPlan {
    if rules.executable_comments && statement.contains("/*!") {
        return LimitPlan::StopFetching;
    }
    let dialect = parser_dialect(rules.dialect);
    let Ok(parsed) = Parser::parse_sql(&*dialect, statement) else {
        return LimitPlan::StopFetching;
    };
    let [Statement::Query(original)] = parsed.as_slice() else {
        return LimitPlan::StopFetching;
    };
    if !simple(original) {
        return LimitPlan::StopFetching;
    }
    let Some((at, clause)) = insertion(rules, statement, limit) else {
        return LimitPlan::StopFetching;
    };
    let mut text = String::with_capacity(statement.len() + clause.len());
    text.push_str(&statement[..at]);
    text.push_str(&clause);
    text.push_str(&statement[at..]);
    if !verified(rules, original, &text, limit) {
        return LimitPlan::StopFetching;
    }
    LimitPlan::Rewritten {
        text,
        inserted_at: at,
        inserted_len: clause.len(),
    }
}

fn simple(q: &Query) -> bool {
    let plain_clauses = q.limit_clause.is_none()
        && q.fetch.is_none()
        && q.locks.is_empty()
        && q.for_clause.is_none()
        && q.settings.is_none()
        && q.format_clause.is_none()
        && q.pipe_operators.is_empty();
    let SetExpr::Select(select) = q.body.as_ref() else {
        return false;
    };
    let mut writes = false;
    let _ = visit_statements(q, |s| {
        if !matches!(s, Statement::Query(_)) {
            writes = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    plain_clauses && select.into.is_none() && select.top.is_none() && !writes
}

/// Where to insert the clause, and the clause text with its leading space.
fn insertion(rules: &Rules, statement: &str, limit: u64) -> Option<(usize, String)> {
    let tokens = tokenize(rules, statement)?;
    match rules.limit {
        LimitStyle::Limit | LimitStyle::FetchFirst => {
            let last = tokens.iter().rev().find(|t| {
                !is_blank(&t.token) && t.token != Token::SemiColon && t.token != Token::EOF
            })?;
            let clause = match rules.limit {
                LimitStyle::Limit => format!(" LIMIT {limit}"),
                _ => format!(" FETCH FIRST {limit} ROWS ONLY"),
            };
            Some((last.range.end, clause))
        }
        LimitStyle::Top => {
            let mut depth = 0i32;
            let mut select_end = None;
            for t in &tokens {
                match &t.token {
                    Token::LParen => depth += 1,
                    Token::RParen => depth -= 1,
                    Token::Word(w) if depth == 0 && w.value.eq_ignore_ascii_case("select") => {
                        select_end = Some(t.range.end);
                        // `SELECT DISTINCT TOP n` / `SELECT ALL TOP n`.
                        let next = tokens
                            .iter()
                            .skip_while(|x| x.range.start < t.range.end)
                            .find(|x| !is_blank(&x.token));
                        if let Some(n) = next
                            && let Token::Word(nw) = &n.token
                            && (nw.value.eq_ignore_ascii_case("distinct")
                                || nw.value.eq_ignore_ascii_case("all"))
                        {
                            select_end = Some(n.range.end);
                        }
                        break;
                    }
                    _ => {}
                }
            }
            Some((select_end?, format!(" TOP {limit}")))
        }
    }
}

/// Parses `text` and checks that it is `original` plus exactly the requested limit.
fn verified(rules: &Rules, original: &Query, text: &str, limit: u64) -> bool {
    let dialect = parser_dialect(rules.dialect);
    let Ok(parsed) = Parser::parse_sql(&*dialect, text) else {
        return false;
    };
    let [Statement::Query(rewritten)] = parsed.as_slice() else {
        return false;
    };
    let mut stripped = rewritten.as_ref().clone();
    let limit_ok = match rules.limit {
        LimitStyle::Limit => {
            let ok = matches!(
                &stripped.limit_clause,
                Some(LimitClause::LimitOffset { limit: Some(e), offset: None, limit_by })
                    if limit_by.is_empty() && is_number(e, limit)
            );
            stripped.limit_clause = None;
            ok
        }
        LimitStyle::FetchFirst => {
            let ok = stripped.fetch.as_ref().is_some_and(|f| {
                !f.with_ties
                    && !f.percent
                    && f.quantity.as_ref().is_some_and(|e| is_number(e, limit))
            });
            stripped.fetch = None;
            ok
        }
        LimitStyle::Top => {
            let SetExpr::Select(select) = stripped.body.as_mut() else {
                return false;
            };
            let ok = select.top.as_ref().is_some_and(|t| {
                !t.with_ties
                    && !t.percent
                    && matches!(&t.quantity, Some(TopQuantity::Constant(n)) if *n == limit)
            });
            select.top = None;
            select.top_before_distinct = false;
            ok
        }
    };
    let mut base = original.clone();
    if let SetExpr::Select(select) = base.body.as_mut() {
        select.top_before_distinct = false;
    }
    limit_ok && stripped == base
}

fn is_number(e: &Expr, n: u64) -> bool {
    match e {
        Expr::Value(v) => v.value.to_string() == n.to_string(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::rules;
    use treasury_core::driver::SqlDialect as D;

    fn rewritten(d: D, s: &str) -> Option<String> {
        match plan(&rules(d), s, 1000) {
            LimitPlan::Rewritten {
                text,
                inserted_at,
                inserted_len,
            } => {
                assert_eq!(&text[..inserted_at], &s[..inserted_at]);
                assert_eq!(&text[inserted_at + inserted_len..], &s[inserted_at..]);
                Some(text)
            }
            _ => None,
        }
    }

    #[test]
    fn limit_dialects_append_after_the_last_token() {
        assert_eq!(
            rewritten(D::Postgres, "select * from t").as_deref(),
            Some("select * from t LIMIT 1000")
        );
        assert_eq!(
            rewritten(
                D::Postgres,
                "select * from t order by a desc -- newest first\n"
            )
            .as_deref(),
            Some("select * from t order by a desc LIMIT 1000 -- newest first\n")
        );
        assert_eq!(
            rewritten(D::MySql, "SELECT a FROM `t`;").as_deref(),
            Some("SELECT a FROM `t` LIMIT 1000;")
        );
        assert_eq!(
            rewritten(D::Snowflake, "with x as (select 1 a) select * from x").as_deref(),
            Some("with x as (select 1 a) select * from x LIMIT 1000")
        );
        assert!(rewritten(D::Databricks, "SELECT * FROM cat.sch.t").is_some());
    }

    #[test]
    fn top_goes_after_select_and_distinct() {
        assert_eq!(
            rewritten(D::MsSql, "SELECT * FROM dbo.t").as_deref(),
            Some("SELECT TOP 1000 * FROM dbo.t")
        );
        assert_eq!(
            rewritten(D::MsSql, "select distinct a from t order by a").as_deref(),
            Some("select distinct TOP 1000 a from t order by a")
        );
        assert_eq!(
            rewritten(D::MsSql, "WITH c AS (SELECT a FROM t) SELECT a FROM c").as_deref(),
            Some("WITH c AS (SELECT a FROM t) SELECT TOP 1000 a FROM c")
        );
    }

    #[test]
    fn fetch_first_for_oracle_and_db2() {
        assert_eq!(
            rewritten(D::Oracle, "SELECT * FROM emp ORDER BY sal").as_deref(),
            Some("SELECT * FROM emp ORDER BY sal FETCH FIRST 1000 ROWS ONLY")
        );
        assert!(rewritten(D::Db2, "select * from sysibm.sysdummy1").is_some());
    }

    #[test]
    fn anything_else_stops_fetching_instead() {
        for (d, s) in [
            (D::Postgres, "select * from t limit 5"),
            (D::Postgres, "select * from t for update"),
            (D::Postgres, "select 1 union select 2"),
            (D::Postgres, "select * into t2 from t"),
            (
                D::Postgres,
                "with d as (delete from t returning *) select * from d",
            ),
            (D::Postgres, "update t set a = 1"),
            (
                D::Postgres,
                "select * from t offset 10 rows fetch first 5 rows only",
            ),
            (D::MsSql, "select top 5 * from t"),
            (D::MySql, "select 1 /*! , 2 */"),
            (D::Postgres, "this is not sql"),
            (D::Postgres, "show search_path"),
        ] {
            assert_eq!(
                plan(&rules(d), s, 1000),
                LimitPlan::StopFetching,
                "{d:?} {s}"
            );
        }
    }
}
