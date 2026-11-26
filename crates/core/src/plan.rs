//! Query plans as a tree plus the engine's raw text.

use serde::{Deserialize, Serialize};

/// Whether the plan is estimated or measured by running the statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplainMode {
    /// The optimizer's estimate. The statement does not run.
    Estimate,
    /// Runs the statement and reports actual rows and times. Only allowed for reads.
    Analyze,
}

/// One operator in a plan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanNode {
    /// Operator name: "Seq Scan", "Index Seek", "COLLSCAN".
    pub operator: String,
    /// What it works on: a table, an index, a predicate.
    pub target: Option<String>,
    /// Estimated rows.
    pub estimated_rows: Option<f64>,
    /// Actual rows (analyze only).
    pub actual_rows: Option<f64>,
    /// Estimated total cost, in the engine's units.
    pub cost: Option<f64>,
    /// Actual time in milliseconds (analyze only).
    pub time_ms: Option<f64>,
    /// Other properties as label and value.
    pub properties: Vec<(String, String)>,
    /// Inputs.
    pub children: Vec<PlanNode>,
}

/// A plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// How it was obtained.
    pub mode: ExplainMode,
    /// The root operator.
    pub root: PlanNode,
    /// The engine's own output (JSON, XML or text), for copying and for the model.
    pub raw: String,
}
