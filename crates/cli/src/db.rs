//! Commands that talk to a database: ping, query and export. They go through the same
//! guardrails as the app: sessions are read-only unless `--write` is given, writes on shared
//! databases show how many rows they change first, and production asks for the connection name.

use anyhow::{Result, anyhow, bail};
use secrecy::SecretString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use treasury_core::analyze::{LimitPlan, StatementClass};
use treasury_core::cache::PageStore;
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, Environment, SecretSlot};
use treasury_core::cursor::{Execution, ExecutionOptions};
use treasury_core::driver::{ConnectContext, Driver, SecretSource, Session};
use treasury_core::exec::{Column, ExecEvent, ExecRequest, Row};
use treasury_core::format::{FormatOptions, display};
use treasury_core::guard::{
    self, BlockReason, Confirmation, DEFAULT_COUNT_THRESHOLD, Decision, Policy,
};
use treasury_core::value::Value;

/// How to reach the database.
#[derive(clap::Args)]
pub struct Conn {
    /// Connection string, for example postgres://user@host:5432/db. Keep the password out of
    /// it: it is read from the variable named by --password-env.
    pub url: String,
    /// The environment variable holding the password.
    #[arg(long, default_value = "TREASURY_PASSWORD")]
    pub password_env: String,
    /// Treat the connection as production: destructive statements need the connection name.
    #[arg(long)]
    pub production: bool,
}

struct Secrets(Option<SecretString>);

impl SecretSource for Secrets {
    fn secret(&self, slot: SecretSlot) -> treasury_core::Result<Option<SecretString>> {
        Ok(match slot {
            SecretSlot::Password => self.0.clone(),
            _ => None,
        })
    }
}

fn driver(engine: Engine) -> Result<Arc<dyn Driver>> {
    Ok(match engine {
        Engine::Postgres => Arc::new(treasury_driver_postgres::PostgresDriver),
        Engine::MySql => Arc::new(treasury_driver_mysql::MySqlDriver::mysql()),
        Engine::MariaDb => Arc::new(treasury_driver_mysql::MySqlDriver::mariadb()),
        Engine::SqlServer => Arc::new(treasury_driver_mssql::MsSqlDriver),
        other => bail!(
            "{} is not available from the command line yet.",
            other.display_name()
        ),
    })
}

fn err(e: treasury_core::Error) -> anyhow::Error {
    anyhow!("{e}")
}

async fn connect(c: &Conn, read_only: bool) -> Result<(Arc<dyn Session>, ConnectionConfig)> {
    let p = treasury_core::uri::parse(&c.url).map_err(err)?;
    let engine = p
        .engine
        .ok_or_else(|| anyhow!("The connection string does not say which engine it is for."))?;
    let host = match &p.endpoint {
        Some(Endpoint::Host { host, .. }) => host.clone(),
        _ => "localhost".to_owned(),
    };
    let name = match &p.database {
        Some(db) => format!("{host}/{db}"),
        None => host.clone(),
    };
    let mut config = ConnectionConfig::new(&name, engine, &host).map_err(err)?;
    if let Some(ep) = p.endpoint {
        config.endpoint = ep;
    }
    config.database = p.database;
    config.user = p.user;
    if let Some(mode) = p.tls {
        config.tls.mode = mode;
    }
    config.tls.accept_invalid = p.accept_invalid_certs;
    config.options = p.options;
    config.environment = if c.production {
        Environment::Production
    } else {
        Environment::Development
    };
    let password = std::env::var(&c.password_env)
        .ok()
        .map(SecretString::from)
        .or(p.password);
    let ctx = ConnectContext {
        secrets: Arc::new(Secrets(password)),
        via: None,
        connect_timeout: Duration::from_secs(15),
        metadata_timeout: Duration::from_secs(30),
        read_only,
    };
    let session = driver(engine)?.connect(&config, ctx).await.map_err(err)?;
    Ok((session, config))
}

/// Connects and reports the server and the round trip.
pub async fn ping(c: &Conn) -> Result<String> {
    let (s, _) = connect(c, true).await?;
    let rtt = s.ping().await.map_err(err)?;
    let info = s.server_info().await.map_err(err)?;
    let _ = s.close().await;
    Ok(format!(
        "{} {} answered in {} ms\n",
        info.product,
        info.version,
        rtt.as_millis()
    ))
}

/// Asks for the connection name at a terminal.
fn confirm_name(policy: &Policy, why: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("{why} Run it at a terminal to type the connection name.");
    }
    eprint!(
        "{why}\nType the connection name ({}) to run it: ",
        policy.connection_name
    );
    std::io::stderr().flush()?;
    let mut typed = String::new();
    std::io::stdin().lock().read_line(&mut typed)?;
    if !guard::name_matches(policy, &typed) {
        bail!("The name did not match. Nothing was run.");
    }
    Ok(())
}

async fn count(s: &dyn Session, query: &str) -> Result<u64> {
    let (rows, _) = collect(s, ExecRequest::new(query), 1).await?;
    match rows.first().and_then(|r| r.first()) {
        Some(Value::I64(n)) => Ok(u64::try_from(*n).unwrap_or(0)),
        Some(Value::U64(n)) => Ok(*n),
        Some(Value::Decimal(d)) => Ok(d.parse().unwrap_or(0)),
        other => bail!("The row count came back as {other:?}."),
    }
}

/// Applies the guardrails to `sql`, asking for what they require.
/// Returns what the statement is, once the guardrails allow it.
async fn review(
    s: &dyn Session,
    config: &ConnectionConfig,
    sql: &str,
    write: bool,
) -> Result<StatementClass> {
    let policy = Policy {
        connection_name: config.name.clone(),
        environment: config.environment,
        read_only: !write,
        enforcement: s.capabilities().read_only,
        count_threshold: DEFAULT_COUNT_THRESHOLD,
    };
    let reviewed = guard::review(&policy, s.analyzer(), sql);
    if reviewed.len() != 1 {
        bail!("Give one statement at a time.");
    }
    let class = reviewed[0].statement.class;
    match &reviewed[0].decision {
        Decision::Allow => Ok(class),
        Decision::Block { reason } => bail!(match reason {
            BlockReason::ReadOnly => "This statement could write. Add --write to run it.",
            BlockReason::ReadOnlyEscape => "This statement would switch read-only off.",
            BlockReason::ProductionForbidden => "This statement is not allowed on production.",
        }),
        Decision::Confirm { steps } => {
            for step in steps {
                match step {
                    Confirmation::CountRows { query } => {
                        let n = count(s, query).await?;
                        eprintln!("This changes {n} rows.");
                        if guard::after_count(&policy, n).is_some() {
                            confirm_name(
                                &policy,
                                &format!("It changes more than {DEFAULT_COUNT_THRESHOLD} rows."),
                            )?;
                        }
                    }
                    Confirmation::TypeName { reason } => {
                        let why = match reason {
                            guard::NameReason::Destructive { .. } => {
                                "This drops, truncates or alters a table on production.".to_owned()
                            }
                            guard::NameReason::Unfiltered => {
                                "This has no WHERE clause, so it changes every row.".to_owned()
                            }
                            guard::NameReason::ManyRows { count } => {
                                format!("This changes {count} rows.")
                            }
                            guard::NameReason::NoPreview { detail } => detail.clone(),
                        };
                        confirm_name(&policy, &why)?;
                    }
                }
            }
            Ok(class)
        }
    }
}

/// Runs `req` and keeps the first result set's columns and at most `limit` rows. Reading stops
/// at the limit; the rest stays on the server.
async fn collect(s: &dyn Session, req: ExecRequest, limit: u64) -> Result<(Vec<Row>, Outcome)> {
    use futures::StreamExt;
    let exec = s
        .execute()
        .ok_or_else(|| anyhow!("This connection cannot run statements."))?;
    let mut stream = exec.execute(req).await.map_err(err)?;
    let mut out = Outcome::default();
    let mut rows = Vec::new();
    let mut sets = 0;
    while let Some(event) = stream.next().await {
        match event.map_err(err)? {
            ExecEvent::ResultStart(cols) => {
                sets += 1;
                if sets == 1 {
                    out.columns = cols;
                }
            }
            ExecEvent::Rows(batch) if sets == 1 => {
                for r in batch {
                    if u64::try_from(rows.len()).unwrap_or(u64::MAX) >= limit {
                        out.truncated = true;
                        break;
                    }
                    rows.push(r);
                }
                if out.truncated {
                    // Dropping the stream stops the statement.
                    break;
                }
            }
            ExecEvent::ResultEnd {
                affected: Some(n), ..
            } => out.affected = Some(out.affected.unwrap_or(0) + n),
            ExecEvent::Notice(n) => out.notices.push(n.message),
            _ => {}
        }
    }
    Ok((rows, out))
}

#[derive(Default)]
struct Outcome {
    columns: Vec<Column>,
    affected: Option<u64>,
    truncated: bool,
    notices: Vec<String>,
}

/// Output formats for `query`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum QueryFormat {
    /// Aligned columns for reading.
    Table,
    /// CSV with a header row.
    Csv,
    /// A JSON array of objects.
    Json,
    /// One JSON object per line.
    Ndjson,
}

/// Runs one statement and prints its result.
pub async fn query(
    c: &Conn,
    sql: &str,
    write: bool,
    limit: u64,
    format: QueryFormat,
) -> Result<String> {
    let (s, config) = connect(c, !write).await?;
    let class = review(s.as_ref(), &config, sql, write).await?;
    // Plain SELECTs get the limit written into them; anything else stops reading at it.
    let text = match s.analyzer().limit(sql, limit) {
        LimitPlan::Rewritten { text, .. } => text,
        _ => sql.to_owned(),
    };
    let (rows, out) = collect(s.as_ref(), ExecRequest::new(text), limit).await?;
    let _ = s.close().await;
    for n in &out.notices {
        eprintln!("{n}");
    }
    if out.columns.is_empty() {
        return Ok(match (class, out.affected) {
            (StatementClass::Write, Some(1)) => "1 row changed.\n".to_owned(),
            (StatementClass::Write, Some(n)) => format!("{n} rows changed.\n"),
            _ => "Done.\n".to_owned(),
        });
    }
    let mut text = match format {
        QueryFormat::Table => table(&out.columns, &rows),
        QueryFormat::Csv => render(&out.columns, &rows, &treasury_export::Format::csv())?,
        QueryFormat::Json => render(&out.columns, &rows, &treasury_export::Format::Json)?,
        QueryFormat::Ndjson => render(&out.columns, &rows, &treasury_export::Format::Ndjson)?,
    };
    if out.truncated || rows.len() as u64 == limit {
        eprintln!("Showing the first {limit} rows. Use --limit to see more.");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn render(columns: &[Column], rows: &[Row], format: &treasury_export::Format) -> Result<String> {
    treasury_export::render_rows(columns, rows, format).map_err(err)
}

/// Columns aligned for a terminal; long values are cut with an ellipsis.
fn table(columns: &[Column], rows: &[Row]) -> String {
    const MAX_WIDTH: usize = 48;
    let opts = FormatOptions::default();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| {
                    let c = display(v, &opts);
                    let t = if c.null {
                        "NULL".to_owned()
                    } else {
                        c.text.replace(['\n', '\t'], " ")
                    };
                    if t.chars().count() > MAX_WIDTH {
                        format!("{}…", t.chars().take(MAX_WIDTH - 1).collect::<String>())
                    } else {
                        t
                    }
                })
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|c| c.name.chars().count().min(MAX_WIDTH))
        .collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let line = |values: Vec<String>| -> String {
        values
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (v, w))| {
                let pad = w.saturating_sub(v.chars().count());
                if columns.get(i).is_some_and(|c| c.kind.is_numeric()) {
                    format!("{}{v}", " ".repeat(pad))
                } else {
                    format!("{v}{}", " ".repeat(pad))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_owned()
    };
    let mut out = String::new();
    out.push_str(&line(columns.iter().map(|c| c.name.clone()).collect()));
    out.push('\n');
    out.push_str(
        &widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    );
    out.push('\n');
    for row in cells {
        out.push_str(&line(row));
        out.push('\n');
    }
    out.push_str(&match rows.len() {
        1 => "(1 row)\n".to_owned(),
        n => format!("({n} rows)\n"),
    });
    out
}

/// File formats for `export`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ExportFormat {
    /// CSV with a header row.
    Csv,
    /// A JSON array of objects.
    Json,
    /// One JSON object per line.
    Ndjson,
    /// Apache Parquet.
    Parquet,
}

/// Streams the result of a read into a file.
pub async fn export(c: &Conn, sql: &str, out: &Path, format: ExportFormat) -> Result<String> {
    let (s, config) = connect(c, true).await?;
    review(s.as_ref(), &config, sql, false).await?;
    let exec = s
        .execute()
        .ok_or_else(|| anyhow!("This connection cannot run statements."))?;
    let stream = exec.execute(ExecRequest::new(sql)).await.map_err(err)?;
    let spill = tempfile::tempdir()?;
    let store = PageStore::new(64 * 1024 * 1024, spill.path().into()).map_err(err)?;
    let execution = Execution::new(stream, store, ExecutionOptions::default());
    let format = match format {
        ExportFormat::Csv => treasury_export::Format::csv(),
        ExportFormat::Json => treasury_export::Format::Json,
        ExportFormat::Ndjson => treasury_export::Format::Ndjson,
        ExportFormat::Parquet => treasury_export::Format::Parquet,
    };
    let report = treasury_export::export_execution(&execution, 0, out, &format, &|_| {})
        .await
        .map_err(err)?;
    let _ = s.close().await;
    Ok(format!(
        "Wrote {} rows ({}) to {}\n",
        report.rows,
        treasury_core::format::human_bytes(report.bytes),
        report.path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use treasury_core::exec::ValueKind;

    #[test]
    fn tables_align_and_number_right() {
        let cols = vec![
            Column::new("id", "int", ValueKind::Integer),
            Column::new("name", "text", ValueKind::Text),
        ];
        let rows = vec![
            vec![Value::I64(7), Value::Text("ada".into())],
            vec![Value::I64(1234), Value::Null],
        ];
        assert_eq!(
            table(&cols, &rows),
            "  id  name\n----  ----\n   7  ada\n1234  NULL\n(2 rows)\n"
        );
    }
}
