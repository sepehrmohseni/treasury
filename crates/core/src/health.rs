//! Overview metrics. A metric the user may not read says which grant it needs; it is never shown
//! as blank or as a made-up zero.

use serde::{Deserialize, Serialize};

/// Which metrics to load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSet {
    /// Cheap metrics, loaded on connect under a short statement timeout.
    Light,
    /// Metrics that scan catalogs or statistics, loaded on request.
    Heavy,
}

/// A metric's value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum MetricValue {
    /// A count.
    Count(u64),
    /// A size in bytes.
    Bytes(u64),
    /// A ratio between 0 and 1.
    Ratio(f64),
    /// A duration in milliseconds.
    Millis(f64),
    /// Free text (a version, a role, a replication state).
    Text(String),
    /// A small table, such as the largest tables or the active sessions.
    Table {
        /// Column headings.
        columns: Vec<String>,
        /// Rows of display text.
        rows: Vec<Vec<String>>,
    },
}

/// Whether the metric could be read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MetricStatus {
    /// Read successfully; `value` holds it.
    Ok {
        /// The value.
        value: MetricValue,
    },
    /// The user lacks the privilege.
    NeedsGrant {
        /// The grant or role that would allow it, as the user would ask a DBA for it.
        grant: String,
    },
    /// The engine or server version does not provide it.
    Unavailable {
        /// Why, in a sentence.
        reason: String,
    },
    /// Reading it failed.
    Failed {
        /// The error message.
        message: String,
    },
}

/// One overview metric.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Stable identifier, used to compare with the previous snapshot.
    pub id: String,
    /// Label shown in the overview.
    pub label: String,
    /// The section it belongs to (Storage, Sessions, Cache, Replication...).
    pub group: String,
    /// Its status and value.
    pub status: MetricStatus,
}
