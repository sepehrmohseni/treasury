//! SQL statement analysis: splitting scripts, classifying statements, adding the default row
//! limit and deriving row-count previews, for every SQL dialect Treasury speaks.

#![forbid(unsafe_code)]

pub mod classify;
mod count;
pub mod dialect;
mod functions;
mod limit;
pub mod render;
pub mod split;
mod tokens;

use classify::Verdict;
use dialect::{Rules, parser_dialect, rules};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;
use treasury_core::analyze::{
    Analyzer, Classified, CountPlan, Destructive, LimitPlan, Placeholder, StatementClass,
};
use treasury_core::driver::SqlDialect;

/// The [`Analyzer`] for one SQL dialect.
#[derive(Clone, Copy, Debug)]
pub struct SqlAnalyzer {
    rules: Rules,
}

impl SqlAnalyzer {
    /// The analyzer for `dialect`.
    pub fn new(dialect: SqlDialect) -> Self {
        Self {
            rules: rules(dialect),
        }
    }

    fn verdict(&self, piece: &split::Piece) -> Verdict {
        if piece.block {
            return block_verdict(&piece.parse_text);
        }
        let dialect = parser_dialect(self.rules.dialect);
        match Parser::parse_sql(&*dialect, &piece.parse_text) {
            Ok(statements) if !statements.is_empty() => statements
                .iter()
                .map(|s| classify::statement(&self.rules, s))
                .reduce(Verdict::merge)
                .unwrap_or_else(|| unknown("The statement is empty.")),
            Ok(_) => unknown("The statement is empty."),
            Err(_) => unknown(
                "Treasury could not parse this statement, so it treats it as one that may write.",
            ),
        }
    }
}

fn unknown(reason: &str) -> Verdict {
    Verdict {
        class: StatementClass::Unknown,
        flags: Default::default(),
        reason: Some(reason.to_owned()),
    }
}

/// `text` without the comments that lead it.
fn without_leading_comments(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("--") {
            text = rest.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(rest) = text.strip_prefix("/*") {
            text = rest.split_once("*/").map_or("", |(_, after)| after);
        } else {
            return text;
        }
    }
}

/// PL/SQL units and procedural blocks are classified as a whole.
fn block_verdict(text: &str) -> Verdict {
    let upper: String = without_leading_comments(text)
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    if upper.starts_with("CREATE") {
        let mut v = Verdict {
            class: StatementClass::Ddl,
            flags: Default::default(),
            reason: None,
        };
        if upper.contains("OR REPLACE") {
            v.flags.destructive = Some(Destructive::Alter);
        }
        return v;
    }
    unknown("A procedural block can run any statement; Treasury treats it as one that may write.")
}

/// The leading words of a statement for confirmation dialogs (`DELETE FROM public.orders`):
/// comments removed, the first two keywords in capitals, everything else as written.
fn summary(rules: &Rules, text: &str) -> String {
    let Some(toks) = tokens::tokenize(rules, text) else {
        return text
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
    };
    let mut out = String::new();
    let mut words = 0usize;
    let mut leading_keywords = true;
    let mut gap = false;
    let mut iter = toks.iter().peekable();
    while let Some(t) = iter.next() {
        if t.token == Token::EOF {
            break;
        }
        if tokens::is_blank(&t.token) {
            gap = !out.is_empty();
            continue;
        }
        if words >= 4 && t.token != Token::Period {
            break;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        match &t.token {
            Token::Word(w) => {
                words += 1;
                let is_kw =
                    w.quote_style.is_none() && w.keyword != sqlparser::keywords::Keyword::NoKeyword;
                if leading_keywords && is_kw && words <= 2 {
                    out.push_str(&w.value.to_ascii_uppercase());
                } else {
                    leading_keywords = false;
                    out.push_str(&text[t.range.clone()]);
                }
            }
            _ => {
                if words >= 4 {
                    // A qualified name continues: `schema.table`.
                    let continues =
                        matches!(iter.peek(), Some(n) if matches!(n.token, Token::Word(_)));
                    if !continues {
                        break;
                    }
                    words -= 1;
                }
                leading_keywords = false;
                out.push_str(&text[t.range.clone()]);
            }
        }
    }
    if out.chars().count() > 60 {
        out = out.chars().take(59).collect::<String>() + "\u{2026}";
    }
    out
}

impl Analyzer for SqlAnalyzer {
    fn classify(&self, script: &str) -> Vec<Classified> {
        split::split(&self.rules, script)
            .into_iter()
            .map(|piece| {
                let v = self.verdict(&piece);
                Classified {
                    summary: summary(&self.rules, &script[piece.range.clone()]),
                    range: piece.range,
                    class: v.class,
                    flags: v.flags,
                    reason: v.reason,
                }
            })
            .collect()
    }

    fn limit(&self, statement: &str, limit: u64) -> LimitPlan {
        limit::plan(&self.rules, statement, limit)
    }

    fn count(&self, statement: &str) -> CountPlan {
        count::plan(&self.rules, statement)
    }

    fn placeholders(&self, statement: &str) -> Vec<Placeholder> {
        let Some(toks) = tokens::tokenize(&self.rules, statement) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut brackets = 0i32;
        for (i, t) in toks.iter().enumerate() {
            match &t.token {
                Token::LBracket => brackets += 1,
                Token::RBracket => brackets -= 1,
                Token::Placeholder(p) => found.push(Placeholder {
                    text: p.clone(),
                    range: t.range.clone(),
                }),
                // `:name` in dialects whose tokenizer has no named placeholders. Inside
                // brackets a colon is an array slice (`a[lo:hi]`), not a placeholder.
                Token::Colon if brackets == 0 => {
                    if let Some(next) = toks.get(i + 1)
                        && next.range.start == t.range.end
                        && matches!(&next.token, Token::Word(w) if w.quote_style.is_none())
                    {
                        found.push(Placeholder {
                            text: statement[t.range.start..next.range.end].to_owned(),
                            range: t.range.start..next.range.end,
                        });
                    }
                }
                _ => {}
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementClass as C;

    fn classes(d: SqlDialect, s: &str) -> Vec<C> {
        SqlAnalyzer::new(d)
            .classify(s)
            .into_iter()
            .map(|c| c.class)
            .collect()
    }

    #[test]
    fn scripts_are_classified_statement_by_statement() {
        assert_eq!(
            classes(
                SqlDialect::Postgres,
                "select 1; update t set a = 1; drop table t; vacuum; frob"
            ),
            vec![C::Read, C::Write, C::Ddl, C::Admin, C::Unknown]
        );
    }

    #[test]
    fn summaries_skip_comments() {
        let a = SqlAnalyzer::new(SqlDialect::Postgres);
        let c = a.classify("/* cleanup */ delete from public.orders where id = 1");
        assert_eq!(c[0].summary, "DELETE FROM public.orders");
        let c = a.classify("update  \"Orders\" set a = 1");
        assert_eq!(c[0].summary, "UPDATE \"Orders\" set a");
    }

    #[test]
    fn placeholders_in_order() {
        let pg = SqlAnalyzer::new(SqlDialect::Postgres);
        let p = pg.placeholders("select * from t where a = $1 and b = $2 and c = '$3'");
        let texts: Vec<&str> = p.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["$1", "$2"]);
        let my = SqlAnalyzer::new(SqlDialect::MySql);
        let p = my.placeholders("select * from t where a = ? and b = :name");
        let texts: Vec<&str> = p.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["?", ":name"]);
        assert_eq!(
            &"select * from t where a = ? and b = :name"[p[1].range.clone()],
            ":name"
        );
    }
}
