//! Overview metrics from serverStatus, dbStats and currentOp.

use crate::session::MongoSession;
use async_trait::async_trait;
use mongodb::bson::{Bson, Document, doc};
use treasury_core::driver::Health;
use treasury_core::health::{Metric, MetricSet, MetricStatus, MetricValue};
use treasury_core::{ErrorKind, Result};

/// The role that opens serverStatus and other users' operations.
const MONITOR: &str = "clusterMonitor";

fn metric(id: &str, label: &str, group: &str, status: MetricStatus) -> Metric {
    Metric {
        id: id.to_owned(),
        label: label.to_owned(),
        group: group.to_owned(),
        status,
    }
}

fn num(d: &Document, path: &[&str]) -> Option<f64> {
    let (last, parents) = path.split_last()?;
    let mut cur = d;
    for p in parents {
        cur = cur.get_document(p).ok()?;
    }
    match cur.get(last)? {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(f) => Some(*f),
        _ => None,
    }
}

fn status_of(v: Option<MetricValue>) -> MetricStatus {
    match v {
        Some(value) => MetricStatus::Ok { value },
        None => MetricStatus::Unavailable {
            reason: "The server does not report it.".into(),
        },
    }
}

fn failed(e: &treasury_core::Error) -> MetricStatus {
    match e.kind {
        ErrorKind::Permission { .. } => MetricStatus::NeedsGrant {
            grant: MONITOR.into(),
        },
        _ => MetricStatus::Failed {
            message: e.message.clone(),
        },
    }
}

fn count(v: Option<f64>) -> Option<MetricValue> {
    v.map(|n| MetricValue::Count(n.max(0.0) as u64))
}

impl MongoSession {
    async fn light(&self) -> Vec<Metric> {
        let mut out = Vec::new();
        match self
            .meta(
                "admin",
                doc! {"serverStatus": 1, "repl": 0, "metrics": 0, "locks": 0},
            )
            .await
        {
            Ok(s) => {
                out.push(metric(
                    "version",
                    "Version",
                    "Server",
                    status_of(
                        s.get_str("version")
                            .ok()
                            .map(|v| MetricValue::Text(v.into())),
                    ),
                ));
                out.push(metric(
                    "uptime",
                    "Up for",
                    "Server",
                    status_of(num(&s, &["uptimeMillis"]).map(MetricValue::Millis)),
                ));
                out.push(metric(
                    "connections",
                    "Connections",
                    "Sessions",
                    status_of(count(num(&s, &["connections", "current"]))),
                ));
                out.push(metric(
                    "available",
                    "Connections available",
                    "Sessions",
                    status_of(count(num(&s, &["connections", "available"]))),
                ));
                let used = num(&s, &["wiredTiger", "cache", "bytes currently in the cache"]);
                let max = num(&s, &["wiredTiger", "cache", "maximum bytes configured"]);
                out.push(metric(
                    "cache_used",
                    "Cache used",
                    "Cache",
                    status_of(match (used, max) {
                        (Some(u), Some(m)) if m > 0.0 => Some(MetricValue::Ratio(u / m)),
                        _ => None,
                    }),
                ));
                for (id, label, key) in [
                    ("inserts", "Inserts", "insert"),
                    ("queries", "Queries", "query"),
                    ("updates", "Updates", "update"),
                    ("deletes", "Deletes", "delete"),
                ] {
                    out.push(metric(
                        id,
                        label,
                        "Activity",
                        status_of(count(num(&s, &["opcounters", key]))),
                    ));
                }
            }
            Err(e) => out.push(metric(
                "server_status",
                "Server status",
                "Server",
                failed(&e),
            )),
        }
        match self.meta(&self.database, doc! {"dbStats": 1}).await {
            Ok(s) => {
                out.push(metric(
                    "database_size",
                    "Database size",
                    "Storage",
                    status_of(
                        num(&s, &["storageSize"]).map(|n| MetricValue::Bytes(n.max(0.0) as u64)),
                    ),
                ));
                out.push(metric(
                    "collections",
                    "Collections",
                    "Storage",
                    status_of(count(num(&s, &["collections"]))),
                ));
            }
            Err(e) => out.push(metric(
                "database_size",
                "Database size",
                "Storage",
                failed(&e),
            )),
        }
        out
    }

    async fn heavy(&self) -> Vec<Metric> {
        let active = match self
            .meta(
                "admin",
                doc! {"currentOp": 1, "active": true, "op": {"$ne": "none"}},
            )
            .await
        {
            Ok(r) => {
                let rows: Vec<Vec<String>> = r
                    .get_array("inprog")
                    .map(|a| {
                        a.iter()
                            .filter_map(Bson::as_document)
                            .filter(|op| op.get_str("desc").is_ok_and(|d| d.starts_with("conn")))
                            .take(20)
                            .map(|op| {
                                vec![
                                    op.get("opid").map(ToString::to_string).unwrap_or_default(),
                                    op.get_str("op").unwrap_or("").to_owned(),
                                    op.get_str("ns").unwrap_or("").to_owned(),
                                    num(op, &["secs_running"])
                                        .map(|s| s.to_string())
                                        .unwrap_or_default(),
                                    op.get_document("command")
                                        .map(|c| c.to_string().chars().take(200).collect())
                                        .unwrap_or_default(),
                                ]
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                MetricStatus::Ok {
                    value: MetricValue::Table {
                        columns: ["Operation", "Type", "Namespace", "Seconds", "Command"]
                            .map(String::from)
                            .to_vec(),
                        rows,
                    },
                }
            }
            Err(e) => failed(&e),
        };
        let largest = match self
            .meta_cursor(
                &self.database,
                doc! {"listCollections": 1, "filter": {"type": "collection"}, "nameOnly": true},
                10_000,
            )
            .await
        {
            Ok(colls) => {
                let mut rows = Vec::new();
                for c in colls
                    .iter()
                    .filter_map(|d| d.get_str("name").ok())
                    .take(200)
                {
                    if let Ok(Some(s)) = self
                        .meta_cursor(&self.database, doc! {"aggregate": c, "pipeline": [{"$collStats": {"storageStats": {}}}], "cursor": {}}, 1)
                        .await
                        .map(|v| v.into_iter().next())
                    {
                        let size = num(&s, &["storageStats", "storageSize"]).unwrap_or(0.0);
                        let docs = num(&s, &["storageStats", "count"]).unwrap_or(0.0);
                        rows.push((size, vec![c.to_owned(), treasury_core::format::human_bytes(size as u64), (docs as u64).to_string()]));
                    }
                }
                rows.sort_by(|a, b| b.0.total_cmp(&a.0));
                MetricStatus::Ok {
                    value: MetricValue::Table {
                        columns: ["Collection", "Size", "Documents"]
                            .map(String::from)
                            .to_vec(),
                        rows: rows.into_iter().take(10).map(|(_, r)| r).collect(),
                    },
                }
            }
            Err(e) => failed(&e),
        };
        vec![
            metric(
                "largest_collections",
                "Largest collections",
                "Storage",
                largest,
            ),
            metric("active_operations", "Active operations", "Sessions", active),
        ]
    }
}

#[async_trait]
impl Health for MongoSession {
    async fn metrics(&self, set: MetricSet) -> Result<Vec<Metric>> {
        self.alive()?;
        Ok(match set {
            MetricSet::Light => self.light().await,
            MetricSet::Heavy => self.heavy().await,
        })
    }
}
