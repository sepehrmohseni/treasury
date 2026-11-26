//! `treasury`: Treasury's command-line tool. It runs the same Rust code as the app, which makes
//! it useful in tests and scripts: classify statements before running them elsewhere, check what
//! a pasted connection string means, preview an import from another client.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;
use treasury_core::analyze::{Analyzer, CountPlan, LimitPlan};
use treasury_core::driver::SqlDialect;
use treasury_sql::SqlAnalyzer;

#[derive(Parser)]
#[command(name = "treasury", version, about = "Treasury's command-line tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Dialect {
    Postgres,
    Mysql,
    Mariadb,
    Mssql,
    Snowflake,
    Databricks,
    Oracle,
    Db2,
}

impl From<Dialect> for SqlDialect {
    fn from(d: Dialect) -> Self {
        match d {
            Dialect::Postgres => SqlDialect::Postgres,
            Dialect::Mysql => SqlDialect::MySql,
            Dialect::Mariadb => SqlDialect::MariaDb,
            Dialect::Mssql => SqlDialect::MsSql,
            Dialect::Snowflake => SqlDialect::Snowflake,
            Dialect::Databricks => SqlDialect::Databricks,
            Dialect::Oracle => SqlDialect::Oracle,
            Dialect::Db2 => SqlDialect::Db2,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportSource {
    Dbeaver,
    Beekeeper,
    Pgadmin,
    Jetbrains,
    Compass,
}

#[derive(Subcommand)]
enum Command {
    /// Classify each statement of a script: read, write, DDL, admin or unknown.
    Classify {
        /// The SQL dialect.
        #[arg(long, value_enum)]
        dialect: Dialect,
        /// Read the script from this file instead of the argument.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// The script.
        sql: Option<String>,
    },
    /// Show how the default row limit would be applied to a statement.
    Limit {
        /// The SQL dialect.
        #[arg(long, value_enum)]
        dialect: Dialect,
        /// The limit.
        #[arg(long, default_value_t = 1000)]
        rows: u64,
        /// The statement.
        sql: String,
    },
    /// Show the row-count preview for an UPDATE or DELETE.
    Count {
        /// The SQL dialect.
        #[arg(long, value_enum)]
        dialect: Dialect,
        /// The statement.
        sql: String,
    },
    /// Show what a pasted connection string fills in. The password is never printed.
    ParseUri {
        /// The connection string.
        uri: String,
    },
    /// Preview the connections an import would bring over. Secrets are counted, not printed.
    Import {
        /// The client to import from; guessed from the file when left out.
        #[arg(long = "from", value_enum)]
        from: Option<ImportSource>,
        /// The export file or folder.
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<String> {
    match cli.command {
        Command::Classify {
            dialect,
            file,
            json,
            sql,
        } => {
            let script = match (file, sql) {
                (Some(f), _) => std::fs::read_to_string(&f)
                    .with_context(|| format!("reading {}", f.display()))?,
                (None, Some(s)) => s,
                (None, None) => bail!("give the script as an argument or with --file"),
            };
            Ok(classify(dialect.into(), &script, json))
        }
        Command::Limit { dialect, rows, sql } => {
            Ok(match SqlAnalyzer::new(dialect.into()).limit(&sql, rows) {
                LimitPlan::Rewritten { text, .. } => format!("{text}\n"),
                LimitPlan::StopFetching => {
                    format!("Not rewritten; reading stops after {rows} rows.\n")
                }
                LimitPlan::NotApplicable => "Returns no rows.\n".into(),
            })
        }
        Command::Count { dialect, sql } => Ok(match SqlAnalyzer::new(dialect.into()).count(&sql) {
            CountPlan::Query(q) => format!("{q}\n"),
            CountPlan::Underivable { reason } => format!("{reason}\n"),
            CountPlan::NotApplicable => "Not an UPDATE or DELETE.\n".into(),
        }),
        Command::ParseUri { uri } => parse_uri(&uri),
        Command::Import { from, path } => import(from, &path),
    }
}

fn classify(dialect: SqlDialect, script: &str, json: bool) -> String {
    let statements = SqlAnalyzer::new(dialect).classify(script);
    if json {
        return serde_json::to_string_pretty(&statements).unwrap_or_default() + "\n";
    }
    let mut out = String::new();
    for s in statements {
        let class = serde_json::to_value(s.class)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        out.push_str(&format!("{class:<8} {}", s.summary));
        if let Some(why) = s.reason {
            out.push_str(&format!("  ({why})"));
        }
        out.push('\n');
    }
    out
}

fn parse_uri(uri: &str) -> Result<String> {
    let p = treasury_core::uri::parse(uri).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k:<10} {v}\n"));
    if let Some(e) = p.engine {
        line("engine", e.display_name().into());
    }
    if let Some(ep) = &p.endpoint {
        line("endpoint", serde_json::to_string(ep).unwrap_or_default());
    }
    if let Some(d) = &p.database {
        line("database", d.clone());
    }
    if let Some(u) = &p.user {
        line("user", u.clone());
    }
    line(
        "password",
        if p.password.is_some() {
            "set".into()
        } else {
            "not set".into()
        },
    );
    if let Some(t) = p.tls {
        line("tls", format!("{t:?}").to_ascii_lowercase());
    }
    if p.accept_invalid_certs {
        line("certs", "not verified".into());
    }
    for (k, v) in &p.options {
        line("option", format!("{k}={v}"));
    }
    for i in &p.ignored {
        line("ignored", i.clone());
    }
    Ok(out)
}

fn import(from: Option<ImportSource>, path: &std::path::Path) -> Result<String> {
    use treasury_import::Source;
    let source = match from {
        Some(ImportSource::Dbeaver) => Source::DBeaver,
        Some(ImportSource::Beekeeper) => Source::Beekeeper,
        Some(ImportSource::Pgadmin) => Source::PgAdmin,
        Some(ImportSource::Jetbrains) => Source::JetBrains,
        Some(ImportSource::Compass) => Source::Compass,
        None => treasury_import::detect(path).with_context(|| {
            format!(
                "cannot tell which client {} comes from; pass --from",
                path.display()
            )
        })?,
    };
    let report = treasury_import::import(path, source).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut out = format!("From {}:\n", source.display_name());
    for i in &report.imported {
        out.push_str(&format!(
            "  {} ({}), {} secret{}\n",
            i.config.name,
            i.config.engine.display_name(),
            i.secrets.len(),
            if i.secrets.len() == 1 { "" } else { "s" }
        ));
        for n in &i.notes {
            out.push_str(&format!("    {n}\n"));
        }
    }
    for s in &report.skipped {
        out.push_str(&format!("  skipped {}: {}\n", s.name, s.reason));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<String> {
        let mut full = vec!["treasury"];
        full.extend_from_slice(args);
        run(Cli::try_parse_from(full)?)
    }

    #[test]
    fn classify_prints_each_statement() {
        let out = cli(&[
            "classify",
            "--dialect",
            "postgres",
            "select 1; delete from t",
        ])
        .unwrap();
        assert_eq!(out, "read     SELECT 1\nwrite    DELETE FROM t\n");
        let json = cli(&["classify", "--dialect", "mysql", "--json", "drop table t"]).unwrap();
        assert!(json.contains("\"class\": \"ddl\""));
    }

    #[test]
    fn limit_and_count() {
        assert_eq!(
            cli(&["limit", "--dialect", "mssql", "select * from t"]).unwrap(),
            "select TOP 1000 * from t\n"
        );
        assert_eq!(
            cli(&[
                "count",
                "--dialect",
                "postgres",
                "delete from t where a = 1"
            ])
            .unwrap(),
            "SELECT COUNT(*) FROM t WHERE a = 1\n"
        );
    }

    #[test]
    fn parse_uri_never_prints_the_password() {
        let out = cli(&[
            "parse-uri",
            "postgres://ana:hunter2@db:5433/app?sslmode=require",
        ])
        .unwrap();
        assert!(!out.contains("hunter2"));
        assert!(out.contains("password   set"));
        assert!(out.contains("certs      not verified"));
    }

    #[test]
    fn import_preview_counts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers.json");
        std::fs::write(&path, r#"{"Servers":{"1":{"Name":"Reporting","Host":"pg","Port":5432,"MaintenanceDB":"postgres","Username":"u"}}}"#).unwrap();
        let out = cli(&["import", path.to_str().unwrap()]).unwrap();
        assert!(out.starts_with("From pgAdmin:\n  Reporting (PostgreSQL), 0 secrets\n"));
        assert!(cli(&["import", dir.path().join("nothing.txt").to_str().unwrap()]).is_err());
    }
}
