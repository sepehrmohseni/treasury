//! Overview metrics from the statistics views.

use crate::session::PgSession;
use async_trait::async_trait;
use std::time::Duration;
use tokio_postgres::Row;
use treasury_core::driver::Health;
use treasury_core::health::{Metric, MetricSet, MetricStatus, MetricValue};
use treasury_core::{ErrorKind, Result};

/// The role that lets a user see other sessions' queries and replication details.
const STATS_ROLE: &str = "pg_read_all_stats";

/// Cheap metrics never wait longer than this.
const LIGHT_LIMIT: Duration = Duration::from_secs(5);

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

fn count(n: i64) -> MetricStatus {
    ok(MetricValue::Count(u64::try_from(n).unwrap_or(0)))
}

impl PgSession {
    /// Rows of a metric query, or the status that explains why there are none.
    async fn metric_rows(
        &self,
        sql: &str,
        limit: Duration,
    ) -> std::result::Result<Vec<Row>, MetricStatus> {
        let failed = |message: String| MetricStatus::Failed { message };
        if let Err(e) = self.alive() {
            return Err(failed(e.message.clone()));
        }
        match tokio::time::timeout(limit, self.client.query(sql, &[])).await {
            Ok(Ok(rows)) => Ok(rows),
            Ok(Err(e)) => {
                let err = self.map(&e);
                Err(match err.kind {
                    ErrorKind::Permission { .. } => MetricStatus::NeedsGrant {
                        grant: STATS_ROLE.to_owned(),
                    },
                    _ => failed(err.message.clone()),
                })
            }
            Err(_) => {
                let _ = self.cancel_query().await;
                Err(failed("It took too long to read.".to_owned()))
            }
        }
    }

    /// Reads one value from the first row.
    async fn single(
        &self,
        sql: &str,
        limit: Duration,
        read: impl FnOnce(&Row) -> std::result::Result<MetricStatus, tokio_postgres::Error>,
    ) -> MetricStatus {
        match self.metric_rows(sql, limit).await {
            Ok(rows) => match rows.first() {
                Some(r) => read(r).unwrap_or_else(|e| MetricStatus::Failed {
                    message: self.map(&e).message.clone(),
                }),
                None => MetricStatus::Unavailable {
                    reason: "The server returned nothing.".to_owned(),
                },
            },
            Err(status) => status,
        }
    }

    /// A small table: every column read as text.
    async fn table(&self, sql: &str, columns: &[&str], limit: Duration) -> MetricStatus {
        match self.metric_rows(sql, limit).await {
            Ok(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for r in &rows {
                    let mut cells = Vec::with_capacity(columns.len());
                    for i in 0..columns.len() {
                        match r.try_get::<_, Option<String>>(i) {
                            Ok(v) => cells.push(v.unwrap_or_default()),
                            Err(e) => {
                                return MetricStatus::Failed {
                                    message: self.map(&e).message.clone(),
                                };
                            }
                        }
                    }
                    out.push(cells);
                }
                ok(MetricValue::Table {
                    columns: columns.iter().map(|c| (*c).to_owned()).collect(),
                    rows: out,
                })
            }
            Err(status) => status,
        }
    }

    /// Whether the user may see other sessions' details.
    async fn sees_all_stats(&self, limit: Duration) -> bool {
        let sql = "SELECT rolsuper OR pg_has_role(current_user, 'pg_read_all_stats', 'MEMBER') \
                   FROM pg_roles WHERE rolname = current_user";
        matches!(
            self.metric_rows(sql, limit)
                .await
                .ok()
                .and_then(|rows| { rows.first().and_then(|r| r.try_get::<_, bool>(0).ok()) }),
            Some(true)
        )
    }

    async fn light(&self) -> Vec<Metric> {
        let limit = self.metadata_timeout.min(LIGHT_LIMIT);
        let mut out = Vec::new();
        out.push(metric(
            "version",
            "Version",
            "Server",
            self.single("SELECT current_setting('server_version')", limit, |r| {
                Ok(ok(MetricValue::Text(r.try_get(0)?)))
            })
            .await,
        ));
        out.push(metric(
            "role",
            "Role",
            "Server",
            self.single("SELECT pg_is_in_recovery()", limit, |r| {
                let standby: bool = r.try_get(0)?;
                Ok(ok(MetricValue::Text(
                    if standby { "standby" } else { "primary" }.to_owned(),
                )))
            })
            .await,
        ));
        out.push(metric(
            "uptime",
            "Up for",
            "Server",
            self.single(
                "SELECT (extract(epoch FROM now() - pg_postmaster_start_time()) * 1000)::float8",
                limit,
                |r| Ok(ok(MetricValue::Millis(r.try_get(0)?))),
            )
            .await,
        ));
        out.push(metric(
            "database_size",
            "Database size",
            "Storage",
            self.single("SELECT pg_database_size(current_database())", limit, |r| {
                let n: i64 = r.try_get(0)?;
                Ok(ok(MetricValue::Bytes(u64::try_from(n).unwrap_or(0))))
            })
            .await,
        ));
        out.push(metric(
            "connections",
            "Connections",
            "Sessions",
            self.single(
                "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend'",
                limit,
                |r| Ok(count(r.try_get(0)?)),
            )
            .await,
        ));
        out.push(metric(
            "max_connections",
            "Connection limit",
            "Sessions",
            self.single(
                "SELECT current_setting('max_connections')::int8",
                limit,
                |r| Ok(count(r.try_get(0)?)),
            )
            .await,
        ));
        out.push(metric(
            "cache_hit",
            "Cache hit ratio",
            "Cache",
            self.single(
                "SELECT blks_hit::float8, blks_read::float8 FROM pg_stat_database \
                 WHERE datname = current_database()",
                limit,
                |r| {
                    let hit: f64 = r.try_get(0)?;
                    let read: f64 = r.try_get(1)?;
                    Ok(if hit + read > 0.0 {
                        ok(MetricValue::Ratio(hit / (hit + read)))
                    } else {
                        MetricStatus::Unavailable {
                            reason: "No blocks have been read since statistics were reset."
                                .to_owned(),
                        }
                    })
                },
            )
            .await,
        ));
        for (id, label, column) in [
            ("commits", "Committed transactions", "xact_commit"),
            ("rollbacks", "Rolled back transactions", "xact_rollback"),
            ("deadlocks", "Deadlocks", "deadlocks"),
        ] {
            let sql =
                format!("SELECT {column} FROM pg_stat_database WHERE datname = current_database()");
            out.push(metric(
                id,
                label,
                "Activity",
                self.single(&sql, limit, |r| Ok(count(r.try_get(0)?))).await,
            ));
        }
        out
    }

    async fn heavy(&self) -> Vec<Metric> {
        let limit = self.metadata_timeout;
        let mut out = Vec::new();
        out.push(metric(
            "largest_tables",
            "Largest tables",
            "Storage",
            self.table(
                "SELECT format('%I.%I', n.nspname, c.relname), \
                        pg_size_pretty(pg_total_relation_size(c.oid)), \
                        CASE WHEN c.reltuples >= 0 THEN c.reltuples::int8::text END \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r', 'm') \
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                   AND n.nspname NOT LIKE 'pg\\_toast%' \
                 ORDER BY pg_total_relation_size(c.oid) DESC LIMIT 10",
                &["Table", "Size", "Rows (estimate)"],
                limit,
            )
            .await,
        ));
        out.push(metric(
            "dead_rows",
            "Most dead rows",
            "Storage",
            self.table(
                "SELECT format('%I.%I', schemaname, relname), n_dead_tup::text, n_live_tup::text, \
                        COALESCE(greatest(last_vacuum, last_autovacuum)::text, 'never') \
                 FROM pg_stat_user_tables WHERE n_dead_tup > 0 \
                 ORDER BY n_dead_tup DESC LIMIT 10",
                &["Table", "Dead rows", "Live rows", "Last vacuum"],
                limit,
            )
            .await,
        ));
        out.push(metric(
            "unused_indexes",
            "Indexes never scanned",
            "Storage",
            self.table(
                "SELECT format('%I.%I', s.schemaname, s.relname), format('%I', s.indexrelname), \
                        pg_size_pretty(pg_relation_size(s.indexrelid)) \
                 FROM pg_stat_user_indexes s JOIN pg_index x ON x.indexrelid = s.indexrelid \
                 WHERE s.idx_scan = 0 AND NOT x.indisunique \
                 ORDER BY pg_relation_size(s.indexrelid) DESC LIMIT 10",
                &["Table", "Index", "Size"],
                limit,
            )
            .await,
        ));

        let privileged = self.sees_all_stats(limit).await;
        let needs = || MetricStatus::NeedsGrant {
            grant: STATS_ROLE.to_owned(),
        };
        out.push(metric(
            "active_sessions",
            "Active sessions",
            "Sessions",
            if privileged {
                self.table(
                    "SELECT pid::text, usename::text, state, \
                            COALESCE(to_char(now() - query_start, 'HH24:MI:SS'), ''), \
                            left(query, 200) \
                     FROM pg_stat_activity \
                     WHERE state IS NOT NULL AND state <> 'idle' AND pid <> pg_backend_pid() \
                     ORDER BY query_start LIMIT 20",
                    &["PID", "User", "State", "Running for", "Query"],
                    limit,
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
            self.single(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE state = 'active' AND now() - query_start > interval '1 minute'",
                limit,
                |r| Ok(count(r.try_get(0)?)),
            )
            .await,
        ));
        out.push(metric(
            "waiting_on_locks",
            "Waiting on a lock",
            "Sessions",
            self.single(
                "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock'",
                limit,
                |r| Ok(count(r.try_get(0)?)),
            )
            .await,
        ));

        let standby = matches!(
            self.metric_rows("SELECT pg_is_in_recovery()", limit)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.try_get::<_, bool>(0).ok())),
            Some(true)
        );
        if standby {
            out.push(metric(
                "replay_lag",
                "Replay lag",
                "Replication",
                self.single(
                    "SELECT (extract(epoch FROM now() - pg_last_xact_replay_timestamp()) * 1000)::float8",
                    limit,
                    |r| {
                        Ok(match r.try_get::<_, Option<f64>>(0)? {
                            Some(ms) => ok(MetricValue::Millis(ms.max(0.0))),
                            None => MetricStatus::Unavailable {
                                reason: "No transaction has been replayed yet.".to_owned(),
                            },
                        })
                    },
                )
                .await,
            ));
        } else {
            out.push(metric(
                "replicas",
                "Replicas",
                "Replication",
                if privileged {
                    self.table(
                        "SELECT COALESCE(application_name, ''), COALESCE(client_addr::text, 'local'), \
                                state, sync_state, COALESCE(replay_lag::text, '') \
                         FROM pg_stat_replication ORDER BY application_name",
                        &["Name", "Address", "State", "Sync", "Replay lag"],
                        limit,
                    )
                    .await
                } else {
                    needs()
                },
            ));
        }
        out
    }
}

#[async_trait]
impl Health for PgSession {
    async fn metrics(&self, set: MetricSet) -> Result<Vec<Metric>> {
        self.alive()?;
        Ok(match set {
            MetricSet::Light => self.light().await,
            MetricSet::Heavy => self.heavy().await,
        })
    }
}
