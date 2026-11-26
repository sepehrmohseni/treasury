//! Overview metrics from the catalog views and dynamic management views.

use crate::session::{MsSqlSession, cell, int};
use async_trait::async_trait;
use treasury_core::driver::Health;
use treasury_core::health::{Metric, MetricSet, MetricStatus, MetricValue};
use treasury_core::{ErrorKind, Result};

/// The permission that opens the server-wide views (sessions, uptime, counters).
const SERVER_STATE: &str = "VIEW SERVER STATE";

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

fn failed(e: &treasury_core::Error) -> MetricStatus {
    match e.kind {
        ErrorKind::Permission { .. } => MetricStatus::NeedsGrant {
            grant: SERVER_STATE.to_owned(),
        },
        _ => MetricStatus::Failed {
            message: e.message.clone(),
        },
    }
}

fn needs() -> MetricStatus {
    MetricStatus::NeedsGrant {
        grant: SERVER_STATE.to_owned(),
    }
}

impl MsSqlSession {
    async fn one(
        &self,
        sql: &str,
        read: impl FnOnce(&tiberius::Row) -> Option<MetricValue>,
    ) -> MetricStatus {
        match self.meta(sql, &[]).await {
            Ok(rows) => match rows.first().and_then(read) {
                Some(v) => ok(v),
                None => MetricStatus::Unavailable {
                    reason: "The server returned nothing.".to_owned(),
                },
            },
            Err(e) => failed(&e),
        }
    }

    async fn table(&self, sql: &str, columns: &[&str]) -> MetricStatus {
        match self.meta(sql, &[]).await {
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
            Err(e) => failed(&e),
        }
    }

    async fn sees_server_state(&self) -> bool {
        matches!(
            self.meta(
                "SELECT HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE')",
                &[]
            )
            .await
            .ok()
            .and_then(|r| r.first().and_then(|r| int(r, 0))),
            Some(1)
        )
    }

    async fn light(&self) -> Vec<Metric> {
        let server = self.sees_server_state().await;
        let count = |n: Option<i64>| n.map(|n| MetricValue::Count(u64::try_from(n).unwrap_or(0)));
        vec![
            metric(
                "version",
                "Version",
                "Server",
                self.one(
                    "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64)) + ' ' + \
                            CAST(SERVERPROPERTY('Edition') AS nvarchar(128))",
                    |r| cell(r, 0).map(MetricValue::Text),
                )
                .await,
            ),
            metric(
                "database_size",
                "Database size",
                "Storage",
                self.one("SELECT CAST(SUM(CAST(size AS bigint)) * 8192 AS bigint) FROM sys.database_files", |r| {
                    int(r, 0).map(|n| MetricValue::Bytes(u64::try_from(n).unwrap_or(0)))
                })
                .await,
            ),
            metric(
                "uptime",
                "Up for",
                "Server",
                if server {
                    self.one(
                        "SELECT CAST(DATEDIFF_BIG(millisecond, sqlserver_start_time, SYSDATETIME()) AS bigint) \
                         FROM sys.dm_os_sys_info",
                        |r| int(r, 0).map(|ms| MetricValue::Millis(ms as f64)),
                    )
                    .await
                } else {
                    needs()
                },
            ),
            metric(
                "connections",
                "Connections",
                "Sessions",
                if server {
                    self.one(
                        "SELECT CAST(COUNT(*) AS bigint) FROM sys.dm_exec_sessions WHERE is_user_process = 1",
                        |r| count(int(r, 0)),
                    )
                    .await
                } else {
                    needs()
                },
            ),
            metric(
                "cache_hit",
                "Buffer cache hit ratio",
                "Cache",
                if server {
                    self.one(
                        "SELECT CAST(a.cntr_value AS float) / NULLIF(b.cntr_value, 0) \
                         FROM sys.dm_os_performance_counters a \
                         JOIN sys.dm_os_performance_counters b ON b.object_name = a.object_name \
                         WHERE a.counter_name = 'Buffer cache hit ratio' \
                           AND b.counter_name = 'Buffer cache hit ratio base'",
                        |r| match crate::value::convert(r.cells().next()?.1) {
                            treasury_core::Value::F64(f) => Some(MetricValue::Ratio(f.clamp(0.0, 1.0))),
                            _ => None,
                        },
                    )
                    .await
                } else {
                    needs()
                },
            ),
        ]
    }

    async fn heavy(&self) -> Vec<Metric> {
        let server = self.sees_server_state().await;
        vec![
            metric(
                "largest_tables",
                "Largest tables",
                "Storage",
                self.table(
                    "SELECT TOP 10 SCHEMA_NAME(t.schema_id) + '.' + t.name, \
                            CAST(CAST(SUM(a.total_pages) * 8 / 1024.0 AS decimal(18, 1)) AS nvarchar(32)) + ' MB', \
                            CAST(MAX(CASE WHEN p.index_id IN (0, 1) THEN p.rows END) AS nvarchar(32)) \
                     FROM sys.tables t \
                     JOIN sys.partitions p ON p.object_id = t.object_id \
                     JOIN sys.allocation_units a ON a.container_id = p.partition_id \
                     GROUP BY t.schema_id, t.name ORDER BY SUM(a.total_pages) DESC",
                    &["Table", "Size", "Rows"],
                )
                .await,
            ),
            metric(
                "active_requests",
                "Active requests",
                "Sessions",
                if server {
                    self.table(
                        "SELECT TOP 20 CAST(r.session_id AS nvarchar(10)), s.login_name, r.status, \
                                CAST(r.total_elapsed_time / 1000 AS nvarchar(20)), \
                                COALESCE(r.wait_type, ''), LEFT(t.text, 200) \
                         FROM sys.dm_exec_requests r \
                         JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id \
                         CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t \
                         WHERE s.is_user_process = 1 AND r.session_id <> @@SPID \
                         ORDER BY r.total_elapsed_time DESC",
                        &["Session", "Login", "Status", "Seconds", "Waiting on", "Statement"],
                    )
                    .await
                } else {
                    needs()
                },
            ),
            metric(
                "blocked",
                "Blocked requests",
                "Sessions",
                if server {
                    self.one(
                        "SELECT CAST(COUNT(*) AS bigint) FROM sys.dm_exec_requests WHERE blocking_session_id <> 0",
                        |r| int(r, 0).map(|n| MetricValue::Count(u64::try_from(n).unwrap_or(0))),
                    )
                    .await
                } else {
                    needs()
                },
            ),
        ]
    }
}

#[async_trait]
impl Health for MsSqlSession {
    async fn metrics(&self, set: MetricSet) -> Result<Vec<Metric>> {
        self.alive()?;
        Ok(match set {
            MetricSet::Light => self.light().await,
            MetricSet::Heavy => self.heavy().await,
        })
    }
}
