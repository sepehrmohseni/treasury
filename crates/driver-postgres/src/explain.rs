//! Plans from `EXPLAIN (FORMAT JSON)`.

use crate::binary::Decoded;
use crate::session::PgSession;
use async_trait::async_trait;
use serde_json::Value as Json;
use treasury_core::driver::Explain;
use treasury_core::plan::{ExplainMode, Plan, PlanNode};
use treasury_core::{Error, Result};

/// Keys shown as the node's own fields rather than as properties.
const HANDLED: &[&str] = &[
    "Node Type",
    "Plans",
    "Plan Rows",
    "Actual Rows",
    "Actual Loops",
    "Total Cost",
    "Startup Cost",
    "Actual Total Time",
    "Actual Startup Time",
    "Relation Name",
    "Schema",
    "Alias",
    "Index Name",
    "CTE Name",
    "Function Name",
    "Parent Relationship",
    "Parallel Aware",
    "Async Capable",
    "Plan Width",
];

fn number(v: Option<&Json>) -> Option<f64> {
    v.and_then(Json::as_f64)
}

fn text(v: &Json) -> Option<String> {
    match v {
        Json::Null => None,
        Json::String(s) => Some(s.clone()),
        Json::Array(items) => Some(items.iter().filter_map(text).collect::<Vec<_>>().join(", ")),
        Json::Bool(b) => Some(if *b { "yes" } else { "no" }.to_owned()),
        Json::Number(n) => Some(n.to_string()),
        Json::Object(_) => Some(v.to_string()),
    }
}

/// One node of PostgreSQL's JSON plan.
fn node(v: &Json) -> PlanNode {
    let get = |k: &str| v.get(k);
    let mut operator = get("Node Type")
        .and_then(Json::as_str)
        .unwrap_or("?")
        .to_owned();
    if let Some(join) = get("Join Type").and_then(Json::as_str)
        && join != "Inner"
    {
        operator = format!("{operator} ({join})");
    }
    let relation = get("Relation Name").and_then(Json::as_str);
    let target = match (relation, get("Index Name").and_then(Json::as_str)) {
        (Some(r), Some(i)) => Some(format!("{r} using {i}")),
        (Some(r), None) => Some(match get("Alias").and_then(Json::as_str) {
            Some(a) if a != r => format!("{r} {a}"),
            _ => r.to_owned(),
        }),
        (None, Some(i)) => Some(i.to_owned()),
        (None, None) => get("CTE Name")
            .or_else(|| get("Function Name"))
            .and_then(Json::as_str)
            .map(str::to_owned),
    };
    // Actual figures are per loop; the plan tree shows totals.
    let loops = number(get("Actual Loops")).unwrap_or(1.0);
    let properties = v
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !HANDLED.contains(&k.as_str()) && k.as_str() != "Join Type")
                .filter_map(|(k, val)| text(val).map(|t| (k.clone(), t)))
                .collect()
        })
        .unwrap_or_default();
    PlanNode {
        operator,
        target,
        estimated_rows: number(get("Plan Rows")),
        actual_rows: number(get("Actual Rows")).map(|r| r * loops),
        cost: number(get("Total Cost")),
        time_ms: number(get("Actual Total Time")).map(|t| t * loops),
        properties,
        children: get("Plans")
            .and_then(Json::as_array)
            .map(|c| c.iter().map(node).collect())
            .unwrap_or_default(),
    }
}

/// The plan in `raw`, the text PostgreSQL returned.
pub(crate) fn parse(raw: &str, mode: ExplainMode) -> Result<Plan> {
    let doc: Json = serde_json::from_str(raw)
        .map_err(|e| Error::protocol(format!("The plan could not be read: {e}")))?;
    let top = doc
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| Error::protocol("The plan is empty."))?;
    let mut root = node(
        top.get("Plan")
            .ok_or_else(|| Error::protocol("The plan has no root."))?,
    );
    for key in ["Planning Time", "Execution Time"] {
        if let Some(ms) = number(top.get(key)) {
            root.properties.push((key.to_owned(), format!("{ms} ms")));
        }
    }
    Ok(Plan {
        mode,
        root,
        raw: serde_json::to_string_pretty(&doc).unwrap_or_else(|_| raw.to_owned()),
    })
}

#[async_trait]
impl Explain for PgSession {
    async fn explain(&self, statement: &str, mode: ExplainMode) -> Result<Plan> {
        let options = match mode {
            ExplainMode::Estimate => "FORMAT JSON",
            ExplainMode::Analyze => "ANALYZE, BUFFERS, FORMAT JSON",
        };
        let sql = format!(
            "EXPLAIN ({options}) {}",
            statement.trim().trim_end_matches(';')
        );
        // The extended protocol takes exactly one statement, so text after a semicolon can
        // never run alongside the plan.
        let rows = match mode {
            ExplainMode::Estimate => self.timed(self.client.query(&sql, &[])).await?,
            // ANALYZE runs the user's statement; it stops on Cancel, not on the metadata limit.
            ExplainMode::Analyze => {
                self.alive()?;
                self.client
                    .query(&sql, &[])
                    .await
                    .map_err(|e| self.map(&e))?
            }
        };
        let mut raw = String::new();
        for row in &rows {
            if let Decoded(treasury_core::Value::Json(j)) = self.col(row, 0)? {
                raw.push_str(&j.text);
            }
        }
        parse(&raw, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANALYZED: &str = r#"[{"Plan": {"Node Type": "Nested Loop", "Join Type": "Left",
        "Total Cost": 12.5, "Plan Rows": 10, "Actual Rows": 3, "Actual Loops": 1,
        "Actual Total Time": 0.5,
        "Plans": [
          {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o", "Total Cost": 1.1,
           "Plan Rows": 10, "Actual Rows": 3, "Actual Loops": 1, "Filter": "(total > 10)",
           "Rows Removed by Filter": 7},
          {"Node Type": "Index Scan", "Relation Name": "customers", "Index Name": "customers_pkey",
           "Plan Rows": 1, "Actual Rows": 1, "Actual Loops": 3, "Actual Total Time": 0.01,
           "Index Cond": "(id = o.customer_id)"}
        ]},
        "Planning Time": 0.2, "Execution Time": 0.7}]"#;

    #[test]
    fn json_plans_become_trees() {
        let p = parse(ANALYZED, ExplainMode::Analyze).unwrap();
        assert_eq!(p.root.operator, "Nested Loop (Left)");
        assert_eq!(p.root.children.len(), 2);
        let scan = &p.root.children[0];
        assert_eq!(scan.target.as_deref(), Some("orders o"));
        assert!(
            scan.properties
                .contains(&("Filter".into(), "(total > 10)".into()))
        );
        let index = &p.root.children[1];
        assert_eq!(
            index.target.as_deref(),
            Some("customers using customers_pkey")
        );
        // Per-loop figures are multiplied out.
        assert_eq!(index.actual_rows, Some(3.0));
        assert!(p.root.properties.iter().any(|(k, _)| k == "Execution Time"));
    }

    #[test]
    fn broken_plans_are_errors() {
        assert!(parse("not json", ExplainMode::Estimate).is_err());
        assert!(parse("[]", ExplainMode::Estimate).is_err());
        assert!(parse("[{}]", ExplainMode::Estimate).is_err());
    }
}
