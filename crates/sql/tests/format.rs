//! Formatting over the classifier corpus: a formatted statement is still the same statement to
//! the guardrails, and most statements of every dialect do get formatted.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use treasury_core::analyze::{Analyzer, script_class};
use treasury_core::driver::SqlDialect;
use treasury_sql::SqlAnalyzer;
use treasury_sql::format::format;

const COMMON: &str = include_str!("corpus/common.txt");
const POSTGRES: &str = include_str!("corpus/postgres.txt");
const MYSQL: &str = include_str!("corpus/mysql.txt");
const MARIADB: &str = include_str!("corpus/mariadb.txt");
const MSSQL: &str = include_str!("corpus/mssql.txt");
const SNOWFLAKE: &str = include_str!("corpus/snowflake.txt");
const DATABRICKS: &str = include_str!("corpus/databricks.txt");
const ORACLE: &str = include_str!("corpus/oracle.txt");
const DB2: &str = include_str!("corpus/db2.txt");

fn statements(files: &[&str]) -> Vec<String> {
    files
        .iter()
        .flat_map(|f| f.lines())
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t').map(|(_, sql)| sql.replace('¶', "\n")))
        .collect()
}

fn check(dialect: SqlDialect, files: &[&str]) {
    let analyzer = SqlAnalyzer::new(dialect);
    let all = statements(files);
    let mut formatted = 0;
    let mut changed_class = Vec::new();
    for sql in &all {
        let Some(f) = format(dialect, sql) else {
            continue;
        };
        formatted += 1;
        let (before, after) = (
            script_class(&analyzer.classify(sql)),
            script_class(&analyzer.classify(&f)),
        );
        if before != after {
            changed_class.push(format!("{before:?} -> {after:?}: {sql}\n=> {f}"));
        }
        // Formatting what is already formatted changes nothing more than spacing.
        assert!(
            format(dialect, &f).is_some(),
            "{dialect:?}: cannot format again: {f}"
        );
    }
    assert!(
        changed_class.is_empty(),
        "{dialect:?}: formatting changed what statements are:\n{}",
        changed_class.join("\n\n")
    );
    // The check that refuses a layout changing any token must not refuse most statements.
    assert!(
        formatted * 10 >= all.len() * 9,
        "{dialect:?}: only {formatted} of {} statements formatted",
        all.len()
    );
}

#[test]
fn postgres() {
    check(SqlDialect::Postgres, &[COMMON, POSTGRES]);
}

#[test]
fn mysql_and_mariadb() {
    check(SqlDialect::MySql, &[COMMON, MYSQL]);
    check(SqlDialect::MariaDb, &[COMMON, MYSQL, MARIADB]);
}

#[test]
fn sql_server() {
    check(SqlDialect::MsSql, &[COMMON, MSSQL]);
}

#[test]
fn warehouses_oracle_and_db2() {
    check(SqlDialect::Snowflake, &[COMMON, SNOWFLAKE]);
    check(SqlDialect::Databricks, &[COMMON, DATABRICKS]);
    check(SqlDialect::Oracle, &[COMMON, ORACLE]);
    check(SqlDialect::Db2, &[COMMON, DB2]);
}
