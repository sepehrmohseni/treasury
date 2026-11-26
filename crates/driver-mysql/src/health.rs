//! Overview metrics from SHOW GLOBAL STATUS, system variables and information_schema.

use crate::session::{MySqlSession, cell, int};
use async_trait::async_trait;
use std::collections::HashMap;
use treasury_core::driver::Health;
use treasury_core::health::{Metric, MetricSet, MetricStatus, MetricValue};
use treasury_core::{ErrorKind, Result};

fn metric(id: &str, label: &str, group: &str, status: MetricStatus) -> Metric {
    Metric {
        id: id.to_owned(),
        label: label.to_owned(),
        group: group.to_owned(),
        status,
    }
}

fn ok(value: MetricValue) -> MetricStatus {
    MetricStatus::Ok { value }
}

fn count(n: Option<i64>) -> MetricStatus {
    match n {
        Some(n) => ok(MetricValue::Count(u64::try_from(n).unwrap_or(0))),
        None => MetricStatus::Unavailable {
            reason: "The server does not report it.".to_owned(),
        },
    }
}

fn failed(e: &treasury_core::Error, grant: &str) -> MetricStatus {
    match e.kind {
        ErrorKind::Permission { .. } => MetricStatus::NeedsGrant {
            grant: grant.to_owned(),
        },
        _ => MetricStatus::Failed {
            message: e.message.clone(),
        },
    }
}

const STATUS: &[&str] = &[
    "Uptime",
    "Threads_connected",
    "Threads_running",
    "Questions",
    "Slow_queries",
    "Innodb_buffer_pool_read_requests",
    "Innodb_buffer_pool_reads",
    "Aborted_connects",
];

impl MySqlSession {
    async fn status(&self) -> Result<HashMap<String, i64>> {
        let names = STATUS
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = self
            .meta_text(&format!(
                "SHOW GLOBAL STATUS WHERE Variable_name IN ({names})"
            ))
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| Some((cell(r, 0)?, int(r, 1)?)))
            .collect())
    }

    /// Whether the current account holds a global privilege.
    async fn holds(&self, privilege: &str) -> bool {
        let sql = "SELECT COUNT(*) FROM information_schema.USER_PRIVILEGES \
                   WHERE PRIVILEGE_TYPE = ? AND GRANTEE = CONCAT('''', \
                   SUBSTRING_INDEX(CURRENT_USER(), '@', 1), '''@''', \
                   SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')";
        let arg = mysql_async::Value::Bytes(privilege.as_bytes().to_vec());
        matches!(
            self.meta(sql, vec![arg]).await.ok().and_then(|r| r.first().and_then(|r| int(r, 0))),
            Some(n) if n > 0
        )
    }

    async fn table(&self, sql: &str, columns: &[&str], grant: &str) -> MetricStatus {
        match self.meta_text(sql).await {
            Ok(rows) => ok(MetricValue::Table {
                columns: columns.iter().map(|c| (*c).to_owned()).collect(),
                rows: rows
                    .iter()
                    .map(|r| {
                        (0..columns.len())
                            .map(|i| cell(r, i).unwrap_or_default())
                            .collect()
                    })
                    .collect(),
            }),
            Err(e) => failed(&e, grant),
        }
    }

    async fn light(&self) -> Vec<Metric> {
        let mut out = Vec::new();
        let vars = self
            .meta_text("SELECT VERSION(), @@max_connections, @@read_only, DATABASE()")
            .await;
        let (version, max_conn, read_only, database) = match &vars {
            Ok(rows) => match rows.first() {
                Some(r) => (
                    ok(MetricValue::Text(cell(r, 0).unwrap_or_default())),
                    count(int(r, 1)),
                    ok(MetricValue::Text(
                        if int(r, 2) == Some(1) {
                            "read-only"
                        } else {
                            "accepts writes"
                        }
                        .to_owned(),
                    )),
                    cell(r, 3),
                ),
                None => {
                    let none = || MetricStatus::Unavailable {
                        reason: "The server returned nothing.".to_owned(),
                    };
                    (none(), none(), none(), None)
                }
            },
            Err(e) => (failed(e, ""), failed(e, ""), failed(e, ""), None),
        };
        out.push(metric("version", "Version", "Server", version));
        out.push(metric("role", "Mode", "Server", read_only));
        match self.status().await {
            Ok(s) => {
                out.push(metric(
                    "uptime",
                    "Up for",
                    "Server",
                    match s.get("Uptime") {
                        Some(secs) => ok(MetricValue::Millis(*secs as f64 * 1_000.0)),
                        None => count(None),
                    },
                ));
                out.push(metric(
                    "connections",
                    "Connections",
                    "Sessions",
                    count(s.get("Threads_connected").copied()),
                ));
                out.push(metric(
                    "max_connections",
                    "Connection limit",
                    "Sessions",
                    max_conn,
                ));
                out.push(metric(
                    "running",
                    "Running statements",
                    "Sessions",
                    count(s.get("Threads_running").copied()),
                ));
                let hit = match (
                    s.get("Innodb_buffer_pool_read_requests"),
                    s.get("Innodb_buffer_pool_reads"),
                ) {
                    (Some(req), Some(reads)) if *req > 0 => {
                        ok(MetricValue::Ratio(1.0 - (*reads as f64 / *req as f64)))
                    }
                    _ => MetricStatus::Unavailable {
                        reason: "InnoDB has not read any pages yet.".to_owned(),
                    },
                };
                out.push(metric("cache_hit", "Buffer pool hit ratio", "Cache", hit));
                out.push(metric(
                    "statements",
                    "Statements run",
                    "Activity",
                    count(s.get("Questions").copied()),
                ));
                out.push(metric(
                    "slow",
                    "Slow statements",
                    "Activity",
                    count(s.get("Slow_queries").copied()),
                ));
            }
            Err(e) => out.push(metric("status", "Server status", "Server", failed(&e, ""))),
        }
        let size = match database {
            None => MetricStatus::Unavailable {
                reason: "No database is selected on this connection.".to_owned(),
            },
            Some(db) => match self
                .meta(
                    "SELECT COALESCE(SUM(DATA_LENGTH + INDEX_LENGTH), 0) FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ?",
                    vec![mysql_async::Value::Bytes(db.into_bytes())],
                )
                .await
            {
                Ok(rows) => ok(MetricValue::Bytes(
                    rows.first()
                        .and_then(|r| int(r, 0))
                        .and_then(|n| u64::try_from(n).ok())
                        .unwrap_or(0),
                )),
                Err(e) => failed(&e, ""),
            },
        };
        out.push(metric("database_size", "Database size", "Storage", size));
        out
    }

    async fn heavy(&self) -> Vec<Metric> {
        let mut out = Vec::new();
        out.push(metric(
            "largest_tables",
            "Largest tables",
            "Storage",
            self.table(
                "SELECT CONCAT(TABLE_SCHEMA, '.', TABLE_NAME), \
                        CONCAT(ROUND((DATA_LENGTH + INDEX_LENGTH) / 1048576, 1), ' MB'), \
                        CAST(TABLE_ROWS AS CHAR) \
                 FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA NOT IN ('mysql', 'sys', 'information_schema', 'performance_schema') \
                   AND TABLE_TYPE = 'BASE TABLE' \
                 ORDER BY DATA_LENGTH + INDEX_LENGTH DESC LIMIT 10",
                &["Table", "Size", "Rows (estimate)"],
                "",
            )
            .await,
        ));
        let process = self.holds("PROCESS").await;
        let needs = || MetricStatus::NeedsGrant {
            grant: "PROCESS".to_owned(),
        };
        out.push(metric(
            "active_sessions",
            "Active sessions",
            "Sessions",
            if process {
                self.table(
                    "SELECT CAST(ID AS CHAR), USER, COMMAND, CAST(TIME AS CHAR), COALESCE(STATE, ''), \
                            COALESCE(LEFT(INFO, 200), '') \
                     FROM information_schema.PROCESSLIST \
                     WHERE COMMAND NOT IN ('Sleep', 'Daemon', 'Binlog Dump') AND ID <> CONNECTION_ID() \
                     ORDER BY TIME DESC LIMIT 20",
                    &["ID", "User", "Command", "Seconds", "State", "Statement"],
                    "PROCESS",
                )
                .await
            } else {
                needs()
            },
        ));
        out.push(metric(
            "long_running",
            "Running over a minute",
            "Sessions",
            if process {
                match self
                    .meta_text(
                        "SELECT COUNT(*) FROM information_schema.PROCESSLIST \
                         WHERE COMMAND = 'Query' AND TIME > 60",
                    )
                    .await
                {
                    Ok(rows) => count(rows.first().and_then(|r| int(r, 0))),
                    Err(e) => failed(&e, "PROCESS"),
                }
            } else {
                needs()
            },
        ));
        let replica = match self.meta_text("SHOW REPLICA STATUS").await {
            Ok(rows) if rows.is_empty() => MetricStatus::Unavailable {
                reason: "This server is not a replica.".to_owned(),
            },
            Ok(rows) => {
                let r = &rows[0];
                let col = |names: &[&str]| -> String {
                    r.columns_ref()
                        .iter()
                        .position(|c| names.contains(&c.name_str().as_ref()))
                        .and_then(|i| cell(r, i))
                        .unwrap_or_default()
                };
                ok(MetricValue::Table {
                    columns: vec![
                        "Source".into(),
                        "I/O thread".into(),
                        "SQL thread".into(),
                        "Seconds behind".into(),
                    ],
                    rows: vec![vec![
                        col(&["Source_Host", "Master_Host"]),
                        col(&["Replica_IO_Running", "Slave_IO_Running"]),
                        col(&["Replica_SQL_Running", "Slave_SQL_Running"]),
                        col(&["Seconds_Behind_Source", "Seconds_Behind_Master"]),
                    ]],
                })
            }
            Err(e) => failed(&e, "REPLICATION CLIENT"),
        };
        out.push(metric("replication", "Replication", "Replication", replica));
        out
    }
}

#[async_trait]
impl Health for MySqlSession {
    async fn metrics(&self, set: MetricSet) -> Result<Vec<Metric>> {
        self.alive()?;
        Ok(match set {
            MetricSet::Light => self.light().await,
            MetricSet::Heavy => self.heavy().await,
        })
    }
}
