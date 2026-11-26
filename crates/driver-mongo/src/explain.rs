//! Plans from the `explain` command: the winning plan's stages as a tree.

use crate::session::{MongoSession, parse_command};
use async_trait::async_trait;
use mongodb::bson::{Bson, Document, doc};
use treasury_core::driver::Explain;
use treasury_core::plan::{ExplainMode, Plan, PlanNode};
use treasury_core::{Error, Result};

fn number(d: &Document, k: &str) -> Option<f64> {
    match d.get(k)? {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(f) => Some(*f),
        _ => None,
    }
}

pub(crate) fn stage(d: &Document) -> PlanNode {
    let mut node = PlanNode {
        operator: d.get_str("stage").unwrap_or("?").to_owned(),
        target: d.get_str("indexName").ok().map(str::to_owned),
        estimated_rows: None,
        actual_rows: number(d, "nReturned"),
        cost: number(d, "cost"),
        time_ms: number(d, "executionTimeMillisEstimate"),
        ..PlanNode::default()
    };
    for (k, v) in d {
        match (k.as_str(), v) {
            ("inputStage" | "outerStage" | "innerStage", Bson::Document(child)) => {
                node.children.push(stage(child))
            }
            ("inputStages", Bson::Array(list)) => {
                node.children
                    .extend(list.iter().filter_map(Bson::as_document).map(stage));
            }
            ("stage" | "indexName" | "nReturned" | "executionTimeMillisEstimate", _) => {}
            (_, Bson::Document(_) | Bson::Array(_)) => {
                node.properties.push((k.clone(), v.to_string()))
            }
            (_, other) => node.properties.push((k.clone(), other.to_string())),
        }
    }
    node
}

/// The plan tree in an explain reply: executed stages when present, else the winning plan.
pub(crate) fn parse(reply: &Document, mode: ExplainMode) -> Result<Plan> {
    let executed = reply
        .get_document("executionStats")
        .ok()
        .and_then(|s| s.get_document("executionStages").ok());
    let planned = reply
        .get_document("queryPlanner")
        .ok()
        .and_then(|q| q.get_document("winningPlan").ok())
        .map(|w| w.get_document("queryPlan").unwrap_or(w));
    // Aggregations that cannot be pushed down explain stage by stage.
    let root = match (executed, planned) {
        (Some(e), _) => stage(e),
        (None, Some(p)) => stage(p),
        (None, None) => match reply.get_array("stages") {
            Ok(stages) => PlanNode {
                operator: "Pipeline".into(),
                children: stages
                    .iter()
                    .filter_map(Bson::as_document)
                    .map(|s| PlanNode {
                        operator: s.keys().next().cloned().unwrap_or_default(),
                        actual_rows: number(s, "nReturned"),
                        ..PlanNode::default()
                    })
                    .collect(),
                ..PlanNode::default()
            },
            Err(_) => return Err(Error::protocol("The explain reply has no plan.")),
        },
    };
    let raw = serde_json::to_string_pretty(&Bson::Document(reply.clone()).into_relaxed_extjson())
        .unwrap_or_else(|_| reply.to_string());
    Ok(Plan { mode, root, raw })
}

#[async_trait]
impl Explain for MongoSession {
    async fn explain(&self, statement: &str, mode: ExplainMode) -> Result<Plan> {
        let (db, mut command) = parse_command(statement, &self.database)?;
        command.remove("comment");
        let verbosity = match mode {
            ExplainMode::Estimate => "queryPlanner",
            ExplainMode::Analyze => "executionStats",
        };
        let reply = self
            .meta(&db, doc! {"explain": command, "verbosity": verbosity})
            .await?;
        parse(&reply, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_nest() {
        let reply = doc! {
            "queryPlanner": {"winningPlan": {"stage": "FETCH", "inputStage": {"stage": "IXSCAN", "indexName": "a_1", "keyPattern": {"a": 1}}}},
            "executionStats": {"executionStages": {"stage": "FETCH", "nReturned": 3, "inputStage": {"stage": "IXSCAN", "indexName": "a_1", "nReturned": 3}}}
        };
        let p = parse(&reply, ExplainMode::Analyze).unwrap();
        assert_eq!(p.root.operator, "FETCH");
        assert_eq!(p.root.actual_rows, Some(3.0));
        assert_eq!(p.root.children[0].target.as_deref(), Some("a_1"));
        let planned = parse(
            &doc! {"queryPlanner": {"winningPlan": {"queryPlan": {"stage": "COLLSCAN"}}}},
            ExplainMode::Estimate,
        )
        .unwrap();
        assert_eq!(planned.root.operator, "COLLSCAN");
        assert!(parse(&doc! {"ok": 1}, ExplainMode::Estimate).is_err());
    }
}
