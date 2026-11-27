//! The tables a statement names, with their aliases, for completing column names.
//!
//! This reads tokens, not a parse tree, so it also works on the half-typed statement under the
//! cursor, which usually does not parse yet.

use crate::dialect::parser_dialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer, Word};
use treasury_core::driver::SqlDialect;

/// A table named in a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableName {
    /// The names before the table's own (schema, or database and schema), as written.
    pub qualifiers: Vec<String>,
    /// The table's name, as written (without quotes).
    pub name: String,
    /// What the statement calls it, if it gives it another name.
    pub alias: Option<String>,
}

/// The tables `statement` names after FROM, JOIN, UPDATE, INTO or TABLE, with their aliases.
pub fn table_names(dialect: SqlDialect, statement: &str) -> Vec<TableName> {
    let d = parser_dialect(dialect);
    let Ok(tokens) = Tokenizer::new(&*d, statement).tokenize() else {
        return Vec::new();
    };
    let tokens: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let starts_list = matches!(
            &tokens[i],
            Token::Word(w) if matches!(w.keyword, Keyword::FROM | Keyword::JOIN | Keyword::UPDATE | Keyword::INTO | Keyword::TABLE)
        );
        i += 1;
        if !starts_list {
            continue;
        }
        // FROM a, b AS x, c y: one or more names, separated by commas.
        while let Some((name, next)) = read_name(&tokens, i) {
            i = next;
            let mut table = name;
            match tokens.get(i) {
                Some(Token::Word(w)) if w.keyword == Keyword::AS => {
                    if let Some(Token::Word(a)) = tokens.get(i + 1) {
                        table.alias = Some(a.value.clone());
                        i += 2;
                    }
                }
                Some(Token::Word(w)) if is_alias(w) => {
                    table.alias = Some(w.value.clone());
                    i += 1;
                }
                _ => {}
            }
            out.push(table);
            if matches!(tokens.get(i), Some(Token::Comma)) {
                i += 1;
            } else {
                break;
            }
        }
    }
    out
}

/// A dotted name starting at token `i`, and the index after it.
fn read_name(tokens: &[Token], mut i: usize) -> Option<(TableName, usize)> {
    let mut parts = Vec::new();
    loop {
        match tokens.get(i) {
            Some(Token::Word(w))
                if w.quote_style.is_some() || w.keyword == Keyword::NoKeyword || !reserved(w) =>
            {
                parts.push(w.value.clone());
                i += 1;
            }
            _ => break,
        }
        if matches!(tokens.get(i), Some(Token::Period)) {
            i += 1;
        } else {
            break;
        }
    }
    let name = parts.pop()?;
    Some((
        TableName {
            qualifiers: parts,
            name,
            alias: None,
        },
        i,
    ))
}

/// A word that cannot be a table's or a column's alias without quotes.
fn reserved(w: &Word) -> bool {
    w.quote_style.is_none()
        && (sqlparser::keywords::RESERVED_FOR_TABLE_ALIAS.contains(&w.keyword)
            || sqlparser::keywords::RESERVED_FOR_COLUMN_ALIAS.contains(&w.keyword))
}

fn is_alias(w: &Word) -> bool {
    !reserved(w)
}

/// `name` as an identifier of `dialect`, quoted only when it must be: when the server would
/// fold its case otherwise, when it has characters an unquoted name cannot have, or when it is
/// a reserved word.
pub fn identifier(dialect: SqlDialect, name: &str) -> String {
    let simple = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let folds = match dialect {
        SqlDialect::Postgres => name.chars().any(|c| c.is_ascii_uppercase()),
        SqlDialect::Snowflake | SqlDialect::Oracle | SqlDialect::Db2 => {
            name.chars().any(|c| c.is_ascii_lowercase())
        }
        _ => false,
    };
    let keyword = Tokenizer::new(&*parser_dialect(dialect), name)
        .tokenize()
        .ok()
        .and_then(|t| match t.as_slice() {
            [Token::Word(w)] => Some(reserved(w)),
            _ => None,
        })
        .unwrap_or(true);
    if simple && !folds && !keyword {
        name.to_owned()
    } else {
        crate::render::ident(dialect, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<(Vec<String>, String, Option<String>)> {
        table_names(SqlDialect::Postgres, sql)
            .into_iter()
            .map(|t| (t.qualifiers, t.name, t.alias))
            .collect()
    }

    #[test]
    fn tables_and_their_aliases() {
        assert_eq!(
            names(
                "select o.id from shop.orders o, users join \"Line Items\" as li on li.o = o.id where"
            ),
            [
                (
                    vec!["shop".to_owned()],
                    "orders".to_owned(),
                    Some("o".to_owned())
                ),
                (vec![], "users".to_owned(), None),
                (vec![], "Line Items".to_owned(), Some("li".to_owned())),
            ]
        );
        assert_eq!(
            names("update accounts set plan = 'free' where id = 1"),
            [(vec![], "accounts".to_owned(), None)]
        );
        // Half typed: what is there so far.
        assert_eq!(
            names("select  from orders where o."),
            [(vec![], "orders".to_owned(), None)]
        );
    }

    #[test]
    fn names_are_quoted_only_when_they_must_be() {
        assert_eq!(identifier(SqlDialect::Postgres, "orders"), "orders");
        assert_eq!(identifier(SqlDialect::Postgres, "Orders"), "\"Orders\"");
        assert_eq!(
            identifier(SqlDialect::Postgres, "line items"),
            "\"line items\""
        );
        assert_eq!(identifier(SqlDialect::Postgres, "order"), "\"order\"");
        assert_eq!(identifier(SqlDialect::MySql, "Orders"), "Orders");
        assert_eq!(identifier(SqlDialect::MySql, "select"), "`select`");
        assert_eq!(identifier(SqlDialect::Snowflake, "orders"), "\"orders\"");
        assert_eq!(identifier(SqlDialect::Snowflake, "ORDERS"), "ORDERS");
        assert_eq!(identifier(SqlDialect::MsSql, "2023"), "[2023]");
    }
}
