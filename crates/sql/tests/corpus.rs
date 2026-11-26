//! The classifier corpus: hand-labelled statements per dialect, each run in several variants
//! (upper case, misleading comments before and after, extra line breaks). Two properties must
//! hold: no statement that may write is ever classified as a read, and every statement gets
//! exactly its label.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use treasury_core::analyze::{Analyzer, StatementClass, script_class};
use treasury_core::driver::SqlDialect;
use treasury_sql::SqlAnalyzer;

const COMMON: &str = include_str!("corpus/common.txt");
const POSTGRES: &str = include_str!("corpus/postgres.txt");
const MYSQL: &str = include_str!("corpus/mysql.txt");
const MARIADB: &str = include_str!("corpus/mariadb.txt");
const MSSQL: &str = include_str!("corpus/mssql.txt");
const SNOWFLAKE: &str = include_str!("corpus/snowflake.txt");
const DATABRICKS: &str = include_str!("corpus/databricks.txt");
const ORACLE: &str = include_str!("corpus/oracle.txt");
const DB2: &str = include_str!("corpus/db2.txt");

#[derive(Clone, Debug)]
struct Case {
    label: StatementClass,
    sql: String,
    variant: &'static str,
}

fn label(s: &str) -> StatementClass {
    match s {
        "R" => StatementClass::Read,
        "W" => StatementClass::Write,
        "D" => StatementClass::Ddl,
        "A" => StatementClass::Admin,
        "U" => StatementClass::Unknown,
        other => panic!("unknown label {other}"),
    }
}

fn load(files: &[&str]) -> Vec<Case> {
    let mut cases = Vec::new();
    for file in files {
        for line in file.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let (l, sql) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("corpus line without a tab: {line}"));
            cases.push(Case {
                label: label(l),
                sql: sql.replace('¶', "\n"),
                variant: "original",
            });
        }
    }
    cases
}

fn variants(base: Vec<Case>) -> Vec<Case> {
    let mut out = Vec::new();
    for c in base {
        let directive = c.sql.to_ascii_uppercase().contains("DELIMITER");
        out.push(Case {
            sql: c.sql.to_uppercase(),
            variant: "upper case",
            ..c.clone()
        });
        if !directive {
            out.push(Case {
                sql: format!("/* drop table x; delete from y */ {}", c.sql),
                variant: "leading comment",
                ..c.clone()
            });
        }
        out.push(Case {
            sql: format!("{}\n-- update t set a = 1; drop table t", c.sql),
            variant: "trailing comment",
            ..c.clone()
        });
        let has_comment = c.sql.contains("--") || c.sql.contains('#');
        if !directive && !has_comment && !c.sql.contains(" / ") {
            out.push(Case {
                sql: c.sql.replace(' ', "\n   "),
                variant: "line breaks",
                ..c.clone()
            });
        }
        out.push(c);
    }
    out
}

fn run(dialect: SqlDialect, files: &[&str]) {
    let cases = variants(load(files));
    assert!(
        cases.len() >= 500,
        "{dialect:?}: only {} labelled statements",
        cases.len()
    );
    let analyzer = SqlAnalyzer::new(dialect);
    let mut unsafe_reads = Vec::new();
    let mut mismatches = Vec::new();
    for c in &cases {
        let got = script_class(&analyzer.classify(&c.sql));
        if c.label != StatementClass::Read && got == StatementClass::Read {
            unsafe_reads.push(format!("  [{}] {:?}: {}", c.variant, c.label, c.sql));
        }
        if got != c.label {
            mismatches.push(format!(
                "  [{}] expected {:?}, got {:?}: {}",
                c.variant, c.label, got, c.sql
            ));
        }
    }
    assert!(
        unsafe_reads.is_empty(),
        "{dialect:?}: statements that may write were classified as reads:\n{}",
        unsafe_reads.join("\n")
    );
    assert!(
        mismatches.is_empty(),
        "{dialect:?}: {} of {} statements were misclassified:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches.join("\n")
    );
}

#[test]
fn postgres() {
    run(SqlDialect::Postgres, &[COMMON, POSTGRES]);
}

#[test]
fn mysql() {
    run(SqlDialect::MySql, &[COMMON, MYSQL]);
}

#[test]
fn mariadb() {
    run(SqlDialect::MariaDb, &[COMMON, MYSQL, MARIADB]);
}

#[test]
fn mssql() {
    run(SqlDialect::MsSql, &[COMMON, MSSQL]);
}

#[test]
fn snowflake() {
    run(SqlDialect::Snowflake, &[COMMON, SNOWFLAKE]);
}

#[test]
fn databricks() {
    run(SqlDialect::Databricks, &[COMMON, DATABRICKS]);
}

#[test]
fn oracle() {
    run(SqlDialect::Oracle, &[COMMON, ORACLE]);
}

#[test]
fn db2() {
    run(SqlDialect::Db2, &[COMMON, DB2]);
}

const ALL: [SqlDialect; 8] = [
    SqlDialect::Postgres,
    SqlDialect::MySql,
    SqlDialect::MariaDb,
    SqlDialect::MsSql,
    SqlDialect::Snowflake,
    SqlDialect::Databricks,
    SqlDialect::Oracle,
    SqlDialect::Db2,
];

/// Single-statement corpus lines usable as script parts.
fn parts() -> Vec<Case> {
    load(&[COMMON])
        .into_iter()
        .filter(|c| !c.sql.contains(';') && !c.sql.contains('\n'))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn analysis_never_panics(s in "\\PC{0,200}", d in 0usize..8) {
        let a = SqlAnalyzer::new(ALL[d]);
        let _ = a.classify(&s);
        let _ = a.limit(&s, 1000);
        let _ = a.count(&s);
        let _ = a.placeholders(&s);
    }

    #[test]
    fn sql_like_noise_never_panics(
        words in proptest::collection::vec(
            prop_oneof![
                Just("select"), Just("from"), Just("where"), Just("'"), Just("\""), Just("$$"),
                Just("/*"), Just("*/"), Just("--"), Just(";"), Just("begin"), Just("end"),
                Just("\n/\n"), Just("\nGO\n"), Just("DELIMITER //\n"), Just("q'["), Just("]'"),
                Just("("), Just(")"), Just("t"), Just("1"), Just("/*!"), Just("`"), Just("["),
            ],
            0..40,
        ),
        d in 0usize..8,
    ) {
        let s = words.join(" ");
        let a = SqlAnalyzer::new(ALL[d]);
        let _ = a.classify(&s);
        let _ = a.limit(&s, 10);
        let _ = a.count(&s);
    }

    #[test]
    fn a_script_is_as_dangerous_as_its_worst_statement(
        picks in proptest::collection::vec(any::<prop::sample::Index>(), 1..6),
        d in 0usize..8,
    ) {
        let parts = parts();
        let chosen: Vec<&Case> = picks.iter().map(|i| &parts[i.index(parts.len())]).collect();
        let script = chosen.iter().map(|c| c.sql.as_str()).collect::<Vec<_>>().join(";\n");
        let expected = chosen.iter().map(|c| c.label).max().unwrap_or(StatementClass::Read);
        let got = script_class(&SqlAnalyzer::new(ALL[d]).classify(&script));
        prop_assert_eq!(got, expected, "{}", script);
    }
}
