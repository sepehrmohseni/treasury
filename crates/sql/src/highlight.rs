//! Highlighting spans for the editor, from the same tokenizer the classifier uses, so the editor
//! colors a statement the way Treasury reads it.

use crate::dialect::rules;
use crate::tokens::tokenize;
use serde::{Deserialize, Serialize};
use sqlparser::tokenizer::{Token, Whitespace};
use std::ops::Range;
use treasury_core::driver::SqlDialect;

/// What a span is, for its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// Reserved and non-reserved keywords.
    Keyword,
    /// Names, quoted or not.
    Identifier,
    /// String literals of every quoting style.
    String,
    /// Numbers.
    Number,
    /// Comments.
    Comment,
    /// Bind placeholders.
    Placeholder,
    /// Operators and punctuation.
    Operator,
    /// Text the tokenizer could not read, such as an unterminated string.
    Plain,
}

/// A colored range of the text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Byte range.
    pub range: Range<usize>,
    /// Its kind.
    pub kind: SpanKind,
}

fn kind(t: &Token) -> Option<SpanKind> {
    Some(match t {
        Token::Word(w)
            if w.keyword != sqlparser::keywords::Keyword::NoKeyword && w.quote_style.is_none() =>
        {
            SpanKind::Keyword
        }
        Token::Word(_) => SpanKind::Identifier,
        Token::Number(..) => SpanKind::Number,
        Token::SingleQuotedString(_)
        | Token::DoubleQuotedString(_)
        | Token::TripleSingleQuotedString(_)
        | Token::TripleDoubleQuotedString(_)
        | Token::DollarQuotedString(_)
        | Token::NationalStringLiteral(_)
        | Token::EscapedStringLiteral(_)
        | Token::UnicodeStringLiteral(_)
        | Token::HexStringLiteral(_)
        | Token::SingleQuotedByteStringLiteral(_)
        | Token::DoubleQuotedByteStringLiteral(_) => SpanKind::String,
        Token::Placeholder(_) => SpanKind::Placeholder,
        Token::Whitespace(
            Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
        ) => SpanKind::Comment,
        Token::Whitespace(_) | Token::EOF => return None,
        _ => SpanKind::Operator,
    })
}

/// The spans of `text`, in order. Text the tokenizer rejects (usually while it is being typed)
/// is highlighted up to the last complete statement and plain after it.
pub fn highlight(dialect: SqlDialect, text: &str) -> Vec<Span> {
    let r = rules(dialect);
    if let Some(tokens) = tokenize(&r, text) {
        return tokens
            .into_iter()
            .filter_map(|t| {
                kind(&t.token).map(|k| Span {
                    range: t.range,
                    kind: k,
                })
            })
            .collect();
    }
    // Highlight the longest prefix that tokenizes, line by line.
    let mut cut = text.len();
    while cut > 0 {
        cut = text[..cut].rfind('\n').unwrap_or(0);
        if let Some(tokens) = tokenize(&r, &text[..cut]) {
            let mut spans: Vec<Span> = tokens
                .into_iter()
                .filter_map(|t| {
                    kind(&t.token).map(|k| Span {
                        range: t.range,
                        kind: k,
                    })
                })
                .collect();
            spans.push(Span {
                range: cut..text.len(),
                kind: SpanKind::Plain,
            });
            return spans;
        }
    }
    vec![Span {
        range: 0..text.len(),
        kind: SpanKind::Plain,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(d: SqlDialect, s: &str) -> Vec<(String, SpanKind)> {
        highlight(d, s)
            .into_iter()
            .map(|sp| (s[sp.range].to_owned(), sp.kind))
            .collect()
    }

    #[test]
    fn tokens_get_kinds() {
        let k = kinds(
            SqlDialect::Postgres,
            "SELECT \"Name\", 42 FROM t -- x\nWHERE a = $1 AND b = 'it''s'",
        );
        assert!(k.contains(&("SELECT".into(), SpanKind::Keyword)));
        assert!(k.contains(&("\"Name\"".into(), SpanKind::Identifier)));
        assert!(k.contains(&("42".into(), SpanKind::Number)));
        assert!(k.contains(&("$1".into(), SpanKind::Placeholder)));
        assert!(k.contains(&("'it''s'".into(), SpanKind::String)));
        assert!(
            k.iter()
                .any(|(t, k)| t.starts_with("-- x") && *k == SpanKind::Comment)
        );
    }

    #[test]
    fn unfinished_text_stays_readable() {
        let s = "SELECT 1;\nSELECT 'unterminated";
        let spans = highlight(SqlDialect::Postgres, s);
        assert!(spans.iter().any(|sp| sp.kind == SpanKind::Keyword));
        assert_eq!(spans.last().unwrap().kind, SpanKind::Plain);
        assert_eq!(spans.last().unwrap().range.end, s.len());
    }
}
