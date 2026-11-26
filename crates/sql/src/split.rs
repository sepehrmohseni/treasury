//! Splitting a script into statements, with the lexical rules of each dialect so that
//! terminators inside strings, quoted identifiers and comments are never mistaken for ends.

use crate::dialect::Rules;
use std::ops::Range;

/// One statement (or T-SQL batch) of a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    /// Byte range of the statement in the script, trimmed of surrounding blanks and the
    /// terminator.
    pub range: Range<usize>,
    /// The text to parse: the statement with MySQL executable-comment markers blanked out so
    /// their contents are parsed as the code MySQL runs. Same length as `range`.
    pub parse_text: String,
    /// The statement is a procedural block (PL/SQL, BEGIN ... END) that is classified as a
    /// whole rather than parsed.
    pub block: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Single { backslash: bool },
    Double { backslash: bool },
    Backtick,
    Bracket,
    Dollar,
    QQuote(char),
    Line,
    Block { depth: u32, executable: bool },
}

/// Splits `script` into statements.
pub fn split(rules: &Rules, script: &str) -> Vec<Piece> {
    Splitter::new(rules, script).run()
}

struct Splitter<'a> {
    rules: &'a Rules,
    s: &'a str,
    b: &'a [u8],
    pieces: Vec<Piece>,
    start: usize,
    delimiter: String,
    /// Byte ranges of executable-comment markers to blank out (`/*!50001` and the matching `*/`).
    blanks: Vec<Range<usize>>,
    dollar_tag: String,
}

impl<'a> Splitter<'a> {
    fn new(rules: &'a Rules, s: &'a str) -> Self {
        Self {
            rules,
            s,
            b: s.as_bytes(),
            pieces: Vec::new(),
            start: 0,
            delimiter: ";".into(),
            blanks: Vec::new(),
            dollar_tag: String::new(),
        }
    }

    fn run(mut self) -> Vec<Piece> {
        let mut i = 0;
        let mut state = State::Normal;
        // Procedural block tracking for the current statement.
        let mut block: Option<BlockMode> = None;
        let mut words_seen = 0usize;
        let mut first_words: Vec<String> = Vec::new();
        while i < self.b.len() {
            let c = self.b[i];
            match state {
                State::Normal => {
                    // Directives and batch separators only count at the start of a line.
                    if self.at_line_start(i) {
                        if let Some(end) = self.directive(i, &mut first_words, words_seen) {
                            i = end;
                            continue;
                        }
                        if self.rules.go_batches
                            && let Some(end) = go_line(self.b, i)
                        {
                            self.push(self.start, i, block.is_some());
                            self.start = end;
                            block = None;
                            words_seen = 0;
                            first_words.clear();
                            i = end;
                            continue;
                        }
                        if self.rules.slash_terminator
                            && let Some(end) = slash_line(self.b, i)
                        {
                            self.push(self.start, i, block.is_some());
                            self.start = end;
                            block = None;
                            words_seen = 0;
                            first_words.clear();
                            i = end;
                            continue;
                        }
                    }
                    if c == b'\'' {
                        state = State::Single {
                            backslash: self.rules.backslash_escapes || self.e_string(i),
                        };
                        i += 1;
                        continue;
                    }
                    if self.rules.q_quotes
                        && (c == b'q' || c == b'Q')
                        && self.b.get(i + 1) == Some(&b'\'')
                        && !self.ident_before(i)
                        && let Some(&open) = self.b.get(i + 2)
                    {
                        let close = match open {
                            b'[' => ']',
                            b'{' => '}',
                            b'<' => '>',
                            b'(' => ')',
                            other => char::from(other),
                        };
                        state = State::QQuote(close);
                        i += 3;
                        continue;
                    }
                    if c == b'"' {
                        state = State::Double {
                            backslash: self.rules.double_quote_strings
                                && self.rules.backslash_escapes,
                        };
                        i += 1;
                        continue;
                    }
                    if c == b'`' && self.rules.backticks {
                        state = State::Backtick;
                        i += 1;
                        continue;
                    }
                    if c == b'[' && self.rules.brackets {
                        state = State::Bracket;
                        i += 1;
                        continue;
                    }
                    if c == b'$'
                        && self.rules.dollar_quotes
                        && !self.ident_before(i)
                        && let Some(tag_end) = dollar_tag_end(self.b, i)
                    {
                        self.dollar_tag = self.s[i..=tag_end].to_owned();
                        state = State::Dollar;
                        i = tag_end + 1;
                        continue;
                    }
                    if c == b'-' && self.b.get(i + 1) == Some(&b'-') {
                        state = State::Line;
                        i += 2;
                        continue;
                    }
                    if c == b'#' && self.rules.hash_comments {
                        state = State::Line;
                        i += 1;
                        continue;
                    }
                    if c == b'/' && self.b.get(i + 1) == Some(&b'*') {
                        if self.rules.executable_comments && self.b.get(i + 2) == Some(&b'!') {
                            // `/*!` and an optional version number are markers; the body is code.
                            let mut j = i + 3;
                            while j < self.b.len() && self.b[j].is_ascii_digit() {
                                j += 1;
                            }
                            self.blanks.push(i..j);
                            state = State::Block {
                                depth: 1,
                                executable: true,
                            };
                            i = j;
                            continue;
                        }
                        state = State::Block {
                            depth: 1,
                            executable: false,
                        };
                        i += 2;
                        continue;
                    }
                    if c.is_ascii_alphabetic() || c == b'_' {
                        let end = word_end(self.b, i);
                        if !self.ident_before(i) {
                            let word = self.s[i..end].to_ascii_uppercase();
                            words_seen += 1;
                            if words_seen <= 6 {
                                first_words.push(word.clone());
                                if block.is_none() {
                                    block = self.block_start(&first_words, i, end);
                                }
                            }
                            if let Some(mode) = block.as_mut() {
                                mode.word(&word, self.next_word(end).as_deref());
                            }
                        }
                        i = end;
                        continue;
                    }
                    // Statement terminator.
                    if !self.rules.go_batches && self.s[i..].starts_with(self.delimiter.as_str()) {
                        let ends = match &block {
                            None => true,
                            Some(mode) => mode.closed(),
                        };
                        if ends {
                            self.push(self.start, i, block.is_some());
                            i += self.delimiter.len();
                            self.start = i;
                            block = None;
                            words_seen = 0;
                            first_words.clear();
                            continue;
                        }
                    }
                    i += 1;
                }
                State::Single { backslash } => {
                    if backslash && c == b'\\' {
                        i += 2;
                    } else if c == b'\'' {
                        if self.b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Double { backslash } => {
                    if backslash && c == b'\\' {
                        i += 2;
                    } else if c == b'"' {
                        if self.b.get(i + 1) == Some(&b'"') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Backtick => {
                    if c == b'`' {
                        if self.b.get(i + 1) == Some(&b'`') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Bracket => {
                    if c == b']' {
                        if self.b.get(i + 1) == Some(&b']') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Dollar => {
                    if self.s[i..].starts_with(self.dollar_tag.as_str()) {
                        i += self.dollar_tag.len();
                        state = State::Normal;
                    } else {
                        i += 1;
                    }
                }
                State::QQuote(close) => {
                    let mut buf = [0u8; 4];
                    let close_bytes = close.encode_utf8(&mut buf).as_bytes();
                    if self.b[i..].starts_with(close_bytes)
                        && self.b.get(i + close_bytes.len()) == Some(&b'\'')
                    {
                        i += close_bytes.len() + 1;
                        state = State::Normal;
                    } else {
                        i += 1;
                    }
                }
                State::Line => {
                    if c == b'\n' {
                        state = State::Normal;
                    }
                    i += 1;
                }
                State::Block { depth, executable } => {
                    if c == b'*' && self.b.get(i + 1) == Some(&b'/') {
                        if depth == 1 {
                            if executable {
                                self.blanks.push(i..i + 2);
                            }
                            state = State::Normal;
                        } else {
                            state = State::Block {
                                depth: depth - 1,
                                executable,
                            };
                        }
                        i += 2;
                    } else if self.rules.nested_comments
                        && !executable
                        && c == b'/'
                        && self.b.get(i + 1) == Some(&b'*')
                    {
                        state = State::Block {
                            depth: depth + 1,
                            executable,
                        };
                        i += 2;
                    } else if executable {
                        // Code inside `/*! ... */` is lexed like any other code, except that we
                        // only look for its end here; strings inside are rare and harmless.
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        self.push(self.start, self.b.len(), block.is_some());
        self.pieces
    }

    fn at_line_start(&self, i: usize) -> bool {
        let line_start = self.s[..i].rfind('\n').map_or(0, |n| n + 1);
        self.s[line_start..i].trim().is_empty()
    }

    fn ident_before(&self, i: usize) -> bool {
        i > 0 && {
            let p = self.b[i - 1];
            p.is_ascii_alphanumeric() || p == b'_' || p == b'$' || p >= 0x80
        }
    }

    fn e_string(&self, i: usize) -> bool {
        self.rules.dollar_quotes
            && i > 0
            && matches!(self.b[i - 1], b'e' | b'E')
            && !(i > 1 && (self.b[i - 2].is_ascii_alphanumeric() || self.b[i - 2] == b'_'))
    }

    fn next_word(&self, from: usize) -> Option<String> {
        let rest = &self.s[from..];
        let trimmed = rest.trim_start();
        let start = from + (rest.len() - trimmed.len());
        let end = word_end(self.b, start);
        (end > start).then(|| self.s[start..end].to_ascii_uppercase())
    }

    /// A MySQL `DELIMITER x` line at the start of a statement changes the terminator.
    fn directive(
        &mut self,
        i: usize,
        first_words: &mut Vec<String>,
        words_seen: usize,
    ) -> Option<usize> {
        if !self.rules.delimiter_directive || words_seen > 0 {
            return None;
        }
        let line_end = self.s[i..].find('\n').map_or(self.b.len(), |n| i + n);
        let line = self.s[i..line_end].trim();
        let mut parts = line.split_whitespace();
        let kw = parts.next()?;
        if !kw.eq_ignore_ascii_case("delimiter") {
            return None;
        }
        let new = parts.next()?;
        self.delimiter = new.to_owned();
        first_words.clear();
        let next = (line_end + 1).min(self.b.len());
        self.start = next;
        Some(next)
    }

    /// Whether the statement starting with `words` is a procedural block.
    fn block_start(&self, words: &[String], _start: usize, end: usize) -> Option<BlockMode> {
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        let next = self.next_word(end);
        let procedural_dialect = self.rules.slash_terminator
            || matches!(
                self.rules.dialect,
                treasury_core::driver::SqlDialect::Snowflake
                    | treasury_core::driver::SqlDialect::Db2
                    | treasury_core::driver::SqlDialect::Databricks
            );
        match w.as_slice() {
            ["DECLARE"] if procedural_dialect => Some(BlockMode::new(self.rules.slash_terminator)),
            ["BEGIN"] if procedural_dialect => {
                // BEGIN; / BEGIN TRANSACTION / BEGIN WORK start a transaction instead.
                let tx = matches!(
                    next.as_deref(),
                    None | Some("TRANSACTION" | "WORK" | "TRAN" | "NAME" | "ISOLATION" | "READ")
                ) || self.s[end..].trim_start().starts_with(';');
                // The BEGIN itself is counted when the word is fed to the new block.
                (!tx).then(|| BlockMode::new(self.rules.slash_terminator))
            }
            ["CREATE", ..] if self.rules.slash_terminator => {
                let plsql = w.iter().skip(1).take(4).any(|k| {
                    matches!(
                        *k,
                        "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY"
                    )
                });
                plsql.then(|| BlockMode::new(true))
            }
            _ => None,
        }
    }

    fn push(&mut self, from: usize, to: usize, block: bool) {
        let text = &self.s[from..to.min(self.s.len())];
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        let range = from + lead..to - trail;
        if range.is_empty() || only_comments(self.rules, &self.s[range.clone()]) {
            return;
        }
        let mut parse_text = self.s[range.clone()].to_owned();
        for blank in &self.blanks {
            if blank.start >= range.start && blank.end <= range.end {
                let r = blank.start - range.start..blank.end - range.start;
                parse_text.replace_range(r.clone(), &" ".repeat(r.len()));
            }
        }
        self.pieces.push(Piece {
            range,
            parse_text,
            block,
        });
    }
}

/// Depth tracking for BEGIN ... END blocks. PL/SQL units (Oracle) end only at a `/` line.
struct BlockMode {
    depth: i32,
    slash_only: bool,
    pending_end: bool,
    started: bool,
}

impl BlockMode {
    fn new(slash_only: bool) -> Self {
        Self {
            depth: 0,
            slash_only,
            pending_end: false,
            started: false,
        }
    }

    fn word(&mut self, word: &str, next: Option<&str>) {
        if self.pending_end {
            self.pending_end = false;
            // `END IF`, `END LOOP` close constructs that were not counted.
            if matches!(word, "IF" | "LOOP" | "WHILE" | "FOR" | "REPEAT") {
                self.depth += 1;
                return;
            }
            if word == "CASE" {
                return;
            }
        }
        match word {
            "BEGIN" | "CASE" => {
                self.depth += 1;
                self.started = true;
            }
            "END" => {
                self.depth -= 1;
                self.pending_end = next.is_some();
            }
            _ => {}
        }
    }

    fn closed(&self) -> bool {
        !self.slash_only && self.started && self.depth <= 0
    }
}

fn word_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len()
        && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
    {
        i += 1;
    }
    i
}

/// `$tag$` starting at `i`; returns the index of the closing `$` of the tag.
fn dollar_tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(u8::is_ascii_digit) {
        return None; // `$1` is a placeholder.
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j)
}

/// A line holding only `GO` (and an optional count); returns the index after the line.
fn go_line(b: &[u8], i: usize) -> Option<usize> {
    let line_end = b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |n| i + n);
    let line = std::str::from_utf8(&b[i..line_end]).ok()?.trim();
    let mut parts = line.split_whitespace();
    let kw = parts.next()?;
    if !kw.eq_ignore_ascii_case("go") {
        return None;
    }
    match parts.next() {
        None => {}
        Some(n) if n.chars().all(|c| c.is_ascii_digit()) && parts.next().is_none() => {}
        Some(_) => return None,
    }
    Some((line_end + 1).min(b.len()))
}

/// A line holding only `/`.
fn slash_line(b: &[u8], i: usize) -> Option<usize> {
    let line_end = b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |n| i + n);
    let line = std::str::from_utf8(&b[i..line_end]).ok()?.trim();
    (line == "/").then_some((line_end + 1).min(b.len()))
}

fn only_comments(rules: &Rules, text: &str) -> bool {
    let mut rest = text.trim();
    loop {
        if rest.is_empty() {
            return true;
        }
        if let Some(r) = rest.strip_prefix("--") {
            rest = r
                .split_once('\n')
                .map_or("", |(_, after)| after)
                .trim_start();
        } else if rules.hash_comments && rest.starts_with('#') {
            rest = rest
                .split_once('\n')
                .map_or("", |(_, after)| after)
                .trim_start();
        } else if rest.starts_with("/*") && !(rules.executable_comments && rest.starts_with("/*!"))
        {
            match rest.find("*/") {
                Some(end) => rest = rest[end + 2..].trim_start(),
                None => return true,
            }
        } else {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::rules;
    use treasury_core::driver::SqlDialect as D;

    fn texts(d: D, s: &str) -> Vec<String> {
        split(&rules(d), s)
            .into_iter()
            .map(|p| s[p.range].to_owned())
            .collect()
    }

    #[test]
    fn semicolons_in_strings_and_comments_do_not_split() {
        let s = "select ';' as a; -- x; y\nselect \"a;b\" from t /* ; */; select 1";
        assert_eq!(
            texts(D::Postgres, s),
            vec![
                "select ';' as a",
                "-- x; y\nselect \"a;b\" from t /* ; */",
                "select 1"
            ]
        );
    }

    #[test]
    fn postgres_dollar_quotes_and_nested_comments() {
        let s = "create function f() returns int as $body$ begin return 1; end; $body$ language plpgsql; /* a /* b; */ c; */ select $1;";
        let t = texts(D::Postgres, s);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("plpgsql"));
        assert!(t[1].ends_with("select $1"));
        let e = "select E'it\\'s; fine'; select 2";
        assert_eq!(texts(D::Postgres, e).len(), 2);
    }

    #[test]
    fn mysql_delimiter_and_backslashes() {
        let s = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\nDELIMITER ;\nselect 'a\\';b'; select `x;y` from t";
        let t = texts(D::MySql, s);
        assert_eq!(
            t,
            vec![
                "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                "select 'a\\';b'",
                "select `x;y` from t"
            ]
        );
    }

    #[test]
    fn mysql_executable_comments_are_unwrapped_for_parsing() {
        let s = "SELECT 1 /*!50001 , SLEEP(1) */";
        let p = split(&rules(D::MySql), s);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].parse_text.trim_end(), "SELECT 1          , SLEEP(1)");
        assert_eq!(p[0].parse_text.len(), s.len());
    }

    #[test]
    fn tsql_go_batches() {
        let s = "select 1; select 2\nGO\n  go 3  \nselect [a;b] from t\nGO";
        assert_eq!(
            texts(D::MsSql, s),
            vec!["select 1; select 2", "select [a;b] from t"]
        );
        // `go` inside a statement is not a separator.
        assert_eq!(texts(D::MsSql, "select go from t").len(), 1);
    }

    #[test]
    fn oracle_plsql_units_end_at_slash() {
        let s = "BEGIN\n  DELETE FROM t;\n  COMMIT;\nEND;\n/\nselect q'[it's; here]' from dual;\ncreate or replace procedure p as begin null; end;\n/\nselect 1 from dual";
        let p = split(&rules(D::Oracle), s);
        let t: Vec<&str> = p.iter().map(|x| &s[x.range.clone()]).collect();
        assert_eq!(t.len(), 4, "{t:?}");
        assert!(t[0].starts_with("BEGIN") && t[0].ends_with("END;"));
        assert!(p[0].block);
        assert_eq!(t[1], "select q'[it's; here]' from dual");
        assert!(t[2].starts_with("create or replace procedure"));
        assert!(p[2].block);
        assert_eq!(t[3], "select 1 from dual");
    }

    #[test]
    fn snowflake_blocks_and_transactions() {
        let s = "BEGIN TRANSACTION; insert into t values (1); COMMIT; BEGIN LET x := 1; IF (x > 0) THEN SELECT 1; END IF; END; select 2";
        let p = split(&rules(D::Snowflake), s);
        let t: Vec<&str> = p.iter().map(|x| &s[x.range.clone()]).collect();
        assert_eq!(
            t,
            vec![
                "BEGIN TRANSACTION",
                "insert into t values (1)",
                "COMMIT",
                "BEGIN LET x := 1; IF (x > 0) THEN SELECT 1; END IF; END",
                "select 2"
            ]
        );
        assert!(p[3].block);
    }

    #[test]
    fn comment_only_pieces_are_dropped() {
        assert!(texts(D::Postgres, "  -- nothing\n /* here */ ;  ").is_empty());
        assert_eq!(texts(D::MySql, "# note\nselect 1;").len(), 1);
    }

    #[test]
    fn unterminated_input_never_panics() {
        for s in [
            "select 'abc",
            "select $$ x",
            "/* open",
            "select [x",
            "q'[",
            "\"",
            "BEGIN BEGIN",
        ] {
            for d in [
                D::Postgres,
                D::MySql,
                D::MsSql,
                D::Oracle,
                D::Snowflake,
                D::Databricks,
                D::Db2,
            ] {
                let _ = split(&rules(d), s);
            }
        }
    }
}
