//! Rendering identifiers and values as SQL text for a dialect: for INSERT exports, "copy as
//! INSERT" and the statements shown in "Review changes". Executed edits use bind parameters;
//! this text is for people and for files.

use treasury_core::driver::SqlDialect;
use treasury_core::format::{format_date, format_time, format_timestamp, format_uuid, hex};
use treasury_core::json::to_json;
use treasury_core::value::{Timestamp, Value};

/// `name` quoted as an identifier, always, so reserved words and mixed case survive.
pub fn ident(dialect: SqlDialect, name: &str) -> String {
    match dialect {
        SqlDialect::MySql | SqlDialect::MariaDb | SqlDialect::Databricks => {
            format!("`{}`", name.replace('`', "``"))
        }
        SqlDialect::MsSql => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// A qualified table name: `"schema"."table"`.
pub fn table(dialect: SqlDialect, schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", ident(dialect, s), ident(dialect, table)),
        _ => ident(dialect, table),
    }
}

/// A string literal. MySQL, MariaDB and Databricks treat backslashes as escapes in string
/// literals by default, so those are doubled as well.
pub fn string(dialect: SqlDialect, s: &str) -> String {
    let mut body = s.replace('\'', "''");
    if matches!(
        dialect,
        SqlDialect::MySql | SqlDialect::MariaDb | SqlDialect::Databricks | SqlDialect::Snowflake
    ) {
        body = body.replace('\\', "\\\\");
    }
    if dialect == SqlDialect::MsSql {
        format!("N'{body}'")
    } else {
        format!("'{body}'")
    }
}

/// `v` as a literal.
pub fn literal(dialect: SqlDialect, v: &Value) -> String {
    use SqlDialect as D;
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match dialect {
            D::MsSql => if *b { "1" } else { "0" }.into(),
            _ => if *b { "TRUE" } else { "FALSE" }.into(),
        },
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::I128(n) => n.to_string(),
        Value::U128(n) => n.to_string(),
        Value::F32(f) => float(dialect, f64::from(*f)),
        Value::F64(f) => float(dialect, *f),
        Value::Decimal(s) => {
            if s.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
            {
                s.clone()
            } else {
                string(dialect, s)
            }
        }
        Value::Text(s) => string(dialect, s),
        Value::Bytes(b) => {
            let h = hex(&b.bytes);
            match dialect {
                D::Postgres => format!("'\\x{h}'::bytea"),
                D::MsSql => format!("0x{h}"),
                D::Oracle => format!("HEXTORAW('{h}')"),
                D::Db2 => format!("BX'{h}'"),
                D::Snowflake => format!("TO_BINARY('{h}', 'HEX')"),
                D::MySql | D::MariaDb | D::Databricks => format!("X'{h}'"),
            }
        }
        Value::Date(d) => typed(dialect, "DATE", &format_date(*d)),
        Value::Time(t) => typed(dialect, "TIME", &format_time(*t)),
        Value::Timestamp(ts) => typed(dialect, "TIMESTAMP", &format_timestamp(*ts)),
        Value::TimestampTz(tz) => {
            let local = Timestamp {
                secs: tz.utc.secs.saturating_add(i64::from(tz.offset_secs)),
                nanos: tz.utc.nanos,
            };
            let text = format!(
                "{}{}",
                format_timestamp(local),
                treasury_core::format::format_offset(tz.offset_secs)
            );
            match dialect {
                D::MsSql | D::MySql | D::MariaDb => string(dialect, &text),
                _ => format!("TIMESTAMP {}", plain_string(&text)),
            }
        }
        Value::Uuid(u) => string(dialect, &format_uuid(u)),
        Value::Json(j) => string(dialect, &j.text),
        other => {
            let json = to_json(other);
            match json {
                serde_json::Value::String(s) => string(dialect, &s),
                j => string(dialect, &j.to_string()),
            }
        }
    }
}

fn plain_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn typed(dialect: SqlDialect, keyword: &str, text: &str) -> String {
    match dialect {
        // T-SQL and MySQL take plain strings for temporal columns.
        SqlDialect::MsSql | SqlDialect::MySql | SqlDialect::MariaDb => plain_string(text),
        _ => format!("{keyword} {}", plain_string(text)),
    }
}

fn float(dialect: SqlDialect, f: f64) -> String {
    if f.is_finite() {
        let s = f.to_string();
        return s;
    }
    let word = if f.is_nan() {
        "NaN"
    } else if f > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    };
    match dialect {
        SqlDialect::Postgres => format!("'{word}'::float8"),
        _ => "NULL".into(),
    }
}

/// `INSERT INTO table (columns) VALUES (values);`
pub fn insert(
    dialect: SqlDialect,
    schema: Option<&str>,
    table_name: &str,
    columns: &[&str],
    values: &[Value],
) -> String {
    let cols = columns
        .iter()
        .map(|c| ident(dialect, c))
        .collect::<Vec<_>>()
        .join(", ");
    let vals = values
        .iter()
        .map(|v| literal(dialect, v))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {} ({cols}) VALUES ({vals});",
        table(dialect, schema, table_name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqlAnalyzer;
    use SqlDialect as D;
    use treasury_core::analyze::{Analyzer, StatementClass};
    use treasury_core::value::{Blob, Date, TimestampTz};

    #[test]
    fn identifiers_are_quoted_per_dialect() {
        assert_eq!(ident(D::Postgres, "Order \"x\""), "\"Order \"\"x\"\"\"");
        assert_eq!(ident(D::MySql, "a`b"), "`a``b`");
        assert_eq!(ident(D::MsSql, "a]b"), "[a]]b]");
        assert_eq!(table(D::Postgres, Some("public"), "t"), "\"public\".\"t\"");
    }

    #[test]
    fn strings_cannot_break_out() {
        assert_eq!(string(D::Postgres, "it's"), "'it''s'");
        assert_eq!(
            string(D::MySql, "a\\'; drop table t; --"),
            "'a\\\\''; drop table t; --'"
        );
        assert_eq!(string(D::MsSql, "x"), "N'x'");
    }

    #[test]
    fn values_by_dialect() {
        assert_eq!(literal(D::MsSql, &Value::Bool(true)), "1");
        assert_eq!(literal(D::Postgres, &Value::Bool(false)), "FALSE");
        assert_eq!(
            literal(D::Postgres, &Value::Bytes(Blob::full(vec![1, 255]))),
            "'\\x01ff'::bytea"
        );
        assert_eq!(
            literal(D::MySql, &Value::Bytes(Blob::full(vec![1, 255]))),
            "X'01ff'"
        );
        assert_eq!(
            literal(D::Postgres, &Value::Date(Date { days: 0 })),
            "DATE '1970-01-01'"
        );
        assert_eq!(
            literal(D::MsSql, &Value::Date(Date { days: 0 })),
            "'1970-01-01'"
        );
        assert_eq!(
            literal(D::Postgres, &Value::Decimal("1e5; drop".into())),
            "'1e5; drop'"
        );
        assert_eq!(literal(D::Postgres, &Value::F64(f64::NAN)), "'NaN'::float8");
        assert_eq!(
            literal(
                D::Postgres,
                &Value::TimestampTz(TimestampTz {
                    utc: Timestamp { secs: 0, nanos: 0 },
                    offset_secs: 3600
                })
            ),
            "TIMESTAMP '1970-01-01 01:00:00+01:00'"
        );
    }

    /// Every rendered INSERT parses back as one write in its own dialect, so a hostile value
    /// cannot turn into a second statement.
    #[test]
    fn inserts_parse_as_a_single_write() {
        let hostile = Value::Text("x'); DROP TABLE t; -- \\' \" ` ]".into());
        for d in [
            D::Postgres,
            D::MySql,
            D::MariaDb,
            D::MsSql,
            D::Snowflake,
            D::Databricks,
            D::Oracle,
            D::Db2,
        ] {
            let sql = insert(
                d,
                Some("s"),
                "t",
                &["a", "select"],
                &[hostile.clone(), Value::I64(1)],
            );
            let c = SqlAnalyzer::new(d).classify(&sql);
            assert_eq!(c.len(), 1, "{d:?}: {sql}");
            assert_eq!(c[0].class, StatementClass::Write, "{d:?}: {sql}");
        }
    }
}
