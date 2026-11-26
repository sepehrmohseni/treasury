//! Tokens with byte offsets, from sqlparser's tokenizer (which reports lines and columns).

use crate::dialect::{Rules, parser_dialect};
use sqlparser::tokenizer::{Location, Token, Tokenizer, Whitespace};
use std::ops::Range;

/// A token and its byte range in the text.
pub struct Located {
    /// The token.
    pub token: Token,
    /// Byte range.
    pub range: Range<usize>,
}

/// Tokenizes `text`, or `None` if the tokenizer rejects it.
pub fn tokenize(rules: &Rules, text: &str) -> Option<Vec<Located>> {
    let dialect = parser_dialect(rules.dialect);
    let tokens = Tokenizer::new(&*dialect, text)
        .tokenize_with_location()
        .ok()?;
    let index = LineIndex::new(text);
    tokens
        .into_iter()
        .map(|t| {
            let start = index.offset(t.span.start)?;
            let end = index.offset(t.span.end)?;
            Some(Located {
                token: t.token,
                range: start..end.max(start),
            })
        })
        .collect()
}

/// Whether a token is whitespace or a comment.
pub fn is_blank(t: &Token) -> bool {
    matches!(
        t,
        Token::Whitespace(
            Whitespace::Space
                | Whitespace::Newline
                | Whitespace::Tab
                | Whitespace::SingleLineComment { .. }
                | Whitespace::MultiLineComment(_)
        )
    )
}

struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { text, starts }
    }

    /// Byte offset of a 1-based line and character column.
    fn offset(&self, loc: Location) -> Option<usize> {
        if loc.line == 0 {
            return Some(self.text.len());
        }
        let line = usize::try_from(loc.line).ok()?.checked_sub(1)?;
        let start = *self.starts.get(line)?;
        let col = usize::try_from(loc.column).ok()?.checked_sub(1)?;
        let line_text = &self.text[start..];
        let mut chars = line_text.char_indices();
        match chars.nth(col) {
            Some((i, _)) => Some(start + i),
            None => Some(self.text.len()),
        }
    }
}
