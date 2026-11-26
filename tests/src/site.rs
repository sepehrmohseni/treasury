//! The statement table on the project page. It is produced by the real analyzer and
//! guardrails, so the page cannot claim behaviour the code does not have: a test fails when
//! `site/index.html` holds a different copy. Regenerate it with
//! `cargo run -q -p treasury-tests --example site-statements` and replace the lines between the
//! markers.

use treasury_core::analyze::{Analyzer, Classified, Destructive, LimitPlan, StatementClass};
use treasury_core::config::Environment;
use treasury_core::driver::{ReadOnlyEnforcement, SqlDialect};
use treasury_core::guard::{
    self, BlockReason, Confirmation, DEFAULT_COUNT_THRESHOLD, Decision, NameReason, Policy,
};
use treasury_sql::SqlAnalyzer;

/// The line before the generated rows.
pub const START: &str = "<!-- statements:start -->";
/// The line after them.
pub const END: &str = "<!-- statements:end -->";

/// The default row limit the app applies to plain SELECTs.
const ROW_LIMIT: u64 = 1_000;

/// The statements shown, each with the dialect it is read in.
const EXAMPLES: &[(SqlDialect, &str)] = &[
    (
        SqlDialect::Postgres,
        "SELECT id, total FROM orders WHERE status = 'open'",
    ),
    (
        SqlDialect::Postgres,
        "DELETE FROM sessions WHERE expires_at < now()",
    ),
    (SqlDialect::MySql, "UPDATE accounts SET plan = 'free'"),
    (
        SqlDialect::MsSql,
        "SELECT qty FROM stock WITH (UPDLOCK) WHERE sku = 'A-100'",
    ),
    (
        SqlDialect::Postgres,
        "WITH gone AS (DELETE FROM jobs WHERE done RETURNING id) SELECT count(*) FROM gone",
    ),
    (SqlDialect::Snowflake, "DROP TABLE analytics.events_2023"),
    (
        SqlDialect::Postgres,
        "EXPLAIN ANALYZE UPDATE users SET email = lower(email)",
    ),
    (
        SqlDialect::Postgres,
        "SET default_transaction_read_only = off",
    ),
    (
        SqlDialect::Oracle,
        "SELECT name, hired FROM employees ORDER BY hired",
    ),
    (SqlDialect::Postgres, "CALL archive_month('2026-10')"),
];

fn dialect_name(d: SqlDialect) -> &'static str {
    match d {
        SqlDialect::Postgres => "PostgreSQL",
        SqlDialect::MySql => "MySQL",
        SqlDialect::MariaDb => "MariaDB",
        SqlDialect::MsSql => "SQL Server",
        SqlDialect::Snowflake => "Snowflake",
        SqlDialect::Databricks => "Databricks SQL",
        SqlDialect::Oracle => "Oracle",
        SqlDialect::Db2 => "Db2",
    }
}

/// PostgreSQL's driver has the server refuse writes on read-only sessions; the table assumes
/// Treasury's own enforcement for the rest, which is the stricter of the two.
fn enforcement(d: SqlDialect) -> ReadOnlyEnforcement {
    match d {
        SqlDialect::Postgres => ReadOnlyEnforcement::Server,
        _ => ReadOnlyEnforcement::Client,
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn code(s: &str) -> String {
    format!("<code>{}</code>", escape(s))
}

fn class_cell(c: &Classified) -> String {
    let (tag, label) = match c.class {
        StatementClass::Read => ("read", "Read"),
        StatementClass::Write => ("write", "Write"),
        StatementClass::Ddl => ("ddl", "Schema change"),
        StatementClass::Admin => ("admin", "Admin"),
        StatementClass::Unknown => ("unknown", "Unknown"),
    };
    let mut notes = Vec::new();
    let f = &c.flags;
    if f.locks {
        notes.push("takes locks");
    }
    if f.unfiltered {
        notes.push("no WHERE clause");
    }
    if f.executes {
        notes.push("runs what it explains");
    }
    if f.read_only_escape {
        notes.push("turns read-only off");
    }
    match f.destructive {
        Some(Destructive::Drop) => notes.push("drop"),
        Some(Destructive::Truncate) => notes.push("truncate"),
        Some(Destructive::Alter) => notes.push("alter"),
        None => {}
    }
    if c.class == StatementClass::Unknown {
        notes.push("handled as a write");
    }
    let mut cell = format!("<span class=\"tag tag-{tag}\">{label}</span>");
    for n in notes {
        cell.push_str(&format!("<span class=\"note\">{n}</span>"));
    }
    cell
}

fn runs(analyzer: &SqlAnalyzer, c: &Classified, text: &str) -> String {
    if c.class != StatementClass::Read {
        return "Runs.".into();
    }
    match analyzer.limit(text, ROW_LIMIT) {
        LimitPlan::Rewritten { text, .. } => format!("Runs as {}", code(&text)),
        LimitPlan::StopFetching => "Runs, and stops reading at 1,000 rows.".into(),
        LimitPlan::NotApplicable => "Runs.".into(),
    }
}

fn decision_cell(
    analyzer: &SqlAnalyzer,
    c: &Classified,
    text: &str,
    read_only: bool,
    d: &Decision,
) -> String {
    match d {
        Decision::Allow => {
            if read_only && c.class == StatementClass::Unknown {
                "Sent to the server, whose read-only mode refuses any write.".into()
            } else {
                runs(analyzer, c, text)
            }
        }
        Decision::Block { reason } => match reason {
            BlockReason::ReadOnly => "Refused: it could write.".into(),
            BlockReason::ReadOnlyEscape => "Refused: it would switch read-only off.".into(),
            BlockReason::ProductionForbidden => "Refused on production.".into(),
        },
        Decision::Confirm { steps } => {
            let unfiltered = steps.iter().any(|s| {
                matches!(
                    s,
                    Confirmation::TypeName {
                        reason: NameReason::Unfiltered
                    }
                )
            });
            let mut parts = Vec::new();
            for step in steps {
                match step {
                    Confirmation::CountRows { query } if unfiltered => parts.push(format!(
                        "Shows the row count from {} first, then asks you to type the \
                         connection name, because there is no WHERE clause.",
                        code(query)
                    )),
                    Confirmation::CountRows { query } => parts.push(format!(
                        "Shows the row count from {} first. Above {} rows, it also asks you to \
                         type the connection name.",
                        code(query),
                        thousands(DEFAULT_COUNT_THRESHOLD)
                    )),
                    Confirmation::TypeName { reason } => match reason {
                        NameReason::Unfiltered => {}
                        NameReason::Destructive { .. } | NameReason::ManyRows { .. } => {
                            parts.push("Asks you to type the connection name.".into());
                        }
                        NameReason::NoPreview { detail } => {
                            let why = detail
                                .split_once(": ")
                                .map_or(detail.as_str(), |(_, w)| w)
                                .trim_end_matches('.');
                            parts.push(format!(
                                "Asks you to type the connection name: the rows cannot be \
                                 counted first, because {}.",
                                escape(why)
                            ));
                        }
                    },
                }
            }
            parts.join(" ")
        }
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The table rows, one `<tr>` per statement, indented for the page.
pub fn rows() -> String {
    let mut html = String::new();
    for (i, (dialect, sql)) in EXAMPLES.iter().enumerate() {
        let analyzer = SqlAnalyzer::new(*dialect);
        let classified = analyzer.classify(sql);
        let Some(c) = classified.first() else {
            continue;
        };
        let text = sql.get(c.range.clone()).unwrap_or(sql);
        let policy = |read_only| Policy {
            connection_name: "orders-prod".into(),
            environment: Environment::Production,
            read_only,
            enforcement: enforcement(*dialect),
            count_threshold: DEFAULT_COUNT_THRESHOLD,
        };
        let locked = guard::decide(&policy(true), c, || analyzer.count(text));
        let unlocked = guard::decide(&policy(false), c, || analyzer.count(text));
        let locked = decision_cell(&analyzer, c, text, true, &locked);
        let mut unlocked = decision_cell(&analyzer, c, text, false, &unlocked);
        if unlocked == locked {
            unlocked = "<span class=\"same\">Same as read-only.</span>".into();
        }
        html.push_str(&format!(
            "        <tr>\n          <td class=\"n\">{n}</td>\n          <td class=\"stmt\" data-label=\"Statement\"><span class=\"dialect\">{dialect}</span>{sql}</td>\n          <td class=\"class\" data-label=\"Read as\">{class}</td>\n          <td data-label=\"Read-only\">{locked}</td>\n          <td data-label=\"Writes unlocked\">{unlocked}</td>\n        </tr>\n",
            n = i + 1,
            dialect = dialect_name(*dialect),
            sql = code(sql),
            class = class_cell(c),
            locked = locked,
            unlocked = unlocked,
        ));
    }
    html
}

/// The generated block of `page`, between the markers, with line endings normalised.
pub fn block_in(page: &str) -> Option<String> {
    let page = page.replace("\r\n", "\n");
    let start = page.find(START)? + START.len();
    let end = page[start..].find(END)? + start;
    let inner = &page[start..end];
    // The rows sit between the marker line and the indentation before the end marker.
    let inner = inner.strip_prefix('\n').unwrap_or(inner);
    let inner = inner.trim_end_matches(' ');
    Some(inner.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn escaping() {
        assert_eq!(
            escape("a < 'b' & \"c\""),
            "a &lt; &#39;b&#39; &amp; &quot;c&quot;"
        );
    }

    #[test]
    fn block_extraction() {
        let page = format!("x\n    {START}\n<tr></tr>\n    {END}\ny");
        assert_eq!(block_in(&page).as_deref(), Some("<tr></tr>\n"));
    }
}
