//! Laying out statements for reading. sqlformat breaks the lines and indents them; its result is
//! kept only when Treasury reads exactly the same statements from it:
//!
//! - the same tokens, ignoring spaces and line breaks;
//! - the same text wherever spacing matters although the tokenizer does not see it, as in a
//!   Snowflake stage path (`@stage/data.csv`), which must stay glued together;
//! - the same class for every statement, as the guardrails see it.
//!
//! So formatting changes spaces and line breaks and nothing else.

use crate::SqlAnalyzer;
use crate::dialect::parser_dialect;
use sqlformat::{Dialect, FormatOptions, Indent, QueryParams};
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};
use treasury_core::analyze::Analyzer;
use treasury_core::driver::SqlDialect;

/// `text` with line breaks and indentation, or `None` when laying it out would change anything
/// else about it, as when the formatter does not know a construct of the dialect.
pub fn format(dialect: SqlDialect, text: &str) -> Option<String> {
    let options = FormatOptions {
        indent: Indent::Spaces(2),
        lines_between_queries: 2,
        dialect: match dialect {
            SqlDialect::Postgres => Dialect::PostgreSql,
            SqlDialect::MsSql => Dialect::SQLServer,
            _ => Dialect::Generic,
        },
        ..FormatOptions::default()
    };
    let formatted = sqlformat::format(text, &QueryParams::None, &options);
    let formatted = formatted.trim_end().to_owned();
    let tokens = |t: &str| Tokenizer::new(&*parser_dialect(dialect), t).tokenize().ok();
    let (before, after) = (tokens(text)?, tokens(&formatted)?);
    let analyzer = SqlAnalyzer::new(dialect);
    let classes = |t: &str| -> Vec<_> { analyzer.classify(t).iter().map(|s| s.class).collect() };
    let same = meaning(&before) == meaning(&after)
        && glued(&before) == glued(&after)
        && classes(text) == classes(&formatted);
    same.then_some(formatted)
}

/// Runs of tokens with nothing between them that start with `@`: stage paths, variables. Space
/// inside such a run can change what the server reads, though the tokens stay the same.
fn glued(tokens: &[Token]) -> Vec<String> {
    let mut runs = Vec::new();
    let mut run: Option<String> = None;
    for t in tokens {
        let text = t.to_string();
        let blank = matches!(t, Token::Whitespace(_));
        match &mut run {
            Some(r) if !blank => r.push_str(&text),
            Some(_) => runs.extend(run.take()),
            None if text.starts_with('@') => run = Some(text),
            None => {}
        }
    }
    runs.extend(run);
    runs
}

/// The tokens that carry meaning: all but spaces and line breaks, and comments by their words.
fn meaning(tokens: &[Token]) -> Vec<Token> {
    tokens
        .iter()
        .cloned()
        .filter_map(|t| match t {
            Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab) => None,
            Token::Whitespace(Whitespace::SingleLineComment { comment, prefix }) => {
                Some(Token::Whitespace(Whitespace::SingleLineComment {
                    comment: comment.trim().to_owned(),
                    prefix,
                }))
            }
            Token::Whitespace(Whitespace::MultiLineComment(c)) => Some(Token::Whitespace(
                Whitespace::MultiLineComment(c.split_whitespace().collect::<Vec<_>>().join(" ")),
            )),
            other => Some(other),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_get_lines_and_indentation() {
        let f = format(
            SqlDialect::Postgres,
            "select id, name from users where active and id > 10 order by name",
        )
        .unwrap();
        assert!(f.lines().count() > 3, "{f}");
        assert!(f.starts_with("select"), "keywords keep their case: {f}");
        assert!(f.contains("  id,"), "{f}");
    }

    #[test]
    fn literals_comments_and_quoted_names_stay_as_they_are() {
        let sql = "select 'a  b', \"Mixed Name\" -- the  note\nfrom t /* keep */ where x = 'it''s'";
        let f = format(SqlDialect::Postgres, sql).unwrap();
        for part in [
            "'a  b'",
            "\"Mixed Name\"",
            "the  note",
            "/* keep */",
            "'it''s'",
        ] {
            assert!(f.contains(part), "{part} lost in {f}");
        }
    }

    #[test]
    fn stage_paths_stay_glued() {
        // The formatter would put spaces around the slash, which changes the path.
        let sql = "remove @my_stage/data.csv";
        assert!(
            format(SqlDialect::Snowflake, sql).is_none_or(|f| f.contains("@my_stage/data.csv"))
        );
    }

    #[test]
    fn scripts_keep_every_statement() {
        let f = format(
            SqlDialect::MySql,
            "create table t (a int); insert into t values (1)",
        )
        .unwrap();
        assert!(f.contains(";\n\ninsert"), "{f}");
    }
}
