//! Plans from `EXPLAIN FORMAT=JSON`: MySQL's tree format (version 2) where the server has it,
//! and the older nested format MariaDB and older MySQL servers write.

use crate::session::{MySqlSession, cell};
use async_trait::async_trait;
use serde_json::Value as Json;
use treasury_core::config::Engine;
use treasury_core::driver::Explain;
use treasury_core::plan::{ExplainMode, Plan, PlanNode};
use treasury_core::{Error, Result};

fn number(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn scalar(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(if *b { "yes" } else { "no" }.to_owned()),
        Json::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => Some(
            items
                .iter()
                .filter_map(scalar)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    }
}

/// MySQL's version 2 format: every node has an `operation` and its `inputs`.
fn tree_node(v: &Json) -> PlanNode {
    const HANDLED: &[&str] = &[
        "operation",
        "inputs",
        "estimated_rows",
        "estimated_total_cost",
        "actual_rows",
        "actual_loops",
        "actual_last_row_ms",
        "actual_first_row_ms",
        "table_name",
        "query",
    ];
    let loops = number(v.get("actual_loops")).unwrap_or(1.0);
    PlanNode {
        operator: v
            .get("operation")
            .and_then(Json::as_str)
            .unwrap_or("?")
            .to_owned(),
        target: v
            .get("table_name")
            .and_then(Json::as_str)
            .map(str::to_owned),
        estimated_rows: number(v.get("estimated_rows")),
        actual_rows: number(v.get("actual_rows")).map(|r| r * loops),
        cost: number(v.get("estimated_total_cost")),
        time_ms: number(v.get("actual_last_row_ms")).map(|t| t * loops),
        properties: v
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| !HANDLED.contains(&k.as_str()))
                    .filter_map(|(k, val)| scalar(val).map(|t| (k.clone(), t)))
                    .collect()
            })
            .unwrap_or_default(),
        children: v
            .get("inputs")
            .and_then(Json::as_array)
            .map(|c| c.iter().map(tree_node).collect())
            .unwrap_or_default(),
    }
}

fn label(key: &str) -> String {
    let mut s = key.replace(['_', '-'], " ");
    if let Some(first) = s.get(..1) {
        let upper = first.to_uppercase();
        s.replace_range(..1, &upper);
    }
    s
}

/// The nested format: `query_block`, `nested_loop`, `table`, `ordering_operation`... Each object
/// under a key becomes a node named after the key.
fn nested_node(key: &str, v: &Json) -> PlanNode {
    let get = |k: &str| v.get(k);
    let mut node = PlanNode {
        operator: label(key),
        ..PlanNode::default()
    };
    if key == "table" {
        let access = get("access_type").and_then(Json::as_str).unwrap_or("");
        node.operator = match access {
            "ALL" => "Full table scan",
            "index" => "Full index scan",
            "range" => "Index range scan",
            "ref" | "eq_ref" | "ref_or_null" | "const" | "system" => "Index lookup",
            "fulltext" => "Full-text search",
            "" => "Table",
            other => other,
        }
        .to_owned();
        let table = get("table_name").and_then(Json::as_str);
        node.target = match (table, get("key").and_then(Json::as_str)) {
            (Some(t), Some(k)) => Some(format!("{t} using {k}")),
            (Some(t), None) => Some(t.to_owned()),
            (None, _) => None,
        };
        node.estimated_rows = number(get("rows_examined_per_scan")).or_else(|| number(get("rows")));
        let loops = number(get("r_loops")).unwrap_or(1.0);
        node.actual_rows = number(get("r_rows")).map(|r| r * loops);
        node.time_ms = number(get("r_total_time_ms"));
    }
    node.cost = get("cost_info")
        .and_then(|c| number(c.get("prefix_cost")).or_else(|| number(c.get("query_cost"))))
        .or_else(|| number(get("cost")));
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            match val {
                Json::Object(_) if k != "cost_info" => node.children.push(nested_node(k, val)),
                Json::Array(items) if items.iter().any(Json::is_object) => {
                    for item in items {
                        // `nested_loop: [{"table": {...}}, ...]`: unwrap the single key.
                        match item.as_object() {
                            Some(o) if o.len() == 1 => {
                                if let Some((ik, iv)) = o.iter().next() {
                                    node.children.push(nested_node(ik, iv));
                                }
                            }
                            Some(_) => node.children.push(nested_node(k, item)),
                            None => {}
                        }
                    }
                }
                _ => {
                    if let Some(t) = scalar(val) {
                        node.properties.push((k.clone(), t));
                    }
                }
            }
        }
    }
    node
}

pub(crate) fn parse(raw: &str, mode: ExplainMode) -> Result<Plan> {
    let doc: Json = serde_json::from_str(raw)
        .map_err(|e| Error::protocol(format!("The plan could not be read: {e}")))?;
    let root = if let Some(tree) = doc.get("query_plan") {
        tree_node(tree)
    } else if doc.get("operation").is_some() {
        tree_node(&doc)
    } else if let Some(qb) = doc.get("query_block") {
        nested_node("query_block", qb)
    } else {
        return Err(Error::protocol("The plan has no root."));
    };
    Ok(Plan {
        mode,
        root,
        raw: serde_json::to_string_pretty(&doc).unwrap_or_else(|_| raw.to_owned()),
    })
}

#[async_trait]
impl Explain for MySqlSession {
    async fn explain(&self, statement: &str, mode: ExplainMode) -> Result<Plan> {
        let stmt = statement.trim().trim_end_matches(';');
        let sql = match (self.engine, mode) {
            (Engine::MariaDb, ExplainMode::Analyze) => format!("ANALYZE FORMAT=JSON {stmt}"),
            (_, ExplainMode::Analyze) => format!("EXPLAIN ANALYZE FORMAT=JSON {stmt}"),
            (_, ExplainMode::Estimate) => format!("EXPLAIN FORMAT=JSON {stmt}"),
        };
        if self.engine == Engine::MySql {
            // The tree format; servers before 8.3 do not know the setting and keep the old one.
            let _ = self
                .meta_text("SET @@SESSION.explain_json_format_version = 2")
                .await;
        }
        // Prepared, so text after a semicolon can never run with the plan.
        let rows = self.meta(&sql, vec![]).await?;
        let raw = rows
            .first()
            .and_then(|r| cell(r, 0))
            .ok_or_else(|| Error::protocol("The server returned no plan."))?;
        parse(&raw, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_tree_plans() {
        let raw = r#"{"query": "...", "operation": "Nested loop inner join",
            "estimated_rows": 10, "estimated_total_cost": 4.5,
            "inputs": [
              {"operation": "Table scan on o", "table_name": "orders", "access_type": "table",
               "estimated_rows": 10, "actual_rows": 3, "actual_loops": 2, "actual_last_row_ms": 0.5},
              {"operation": "Single-row index lookup on c using PRIMARY (id = o.customer_id)",
               "table_name": "customers", "estimated_rows": 1}
            ]}"#;
        let p = parse(raw, ExplainMode::Analyze).unwrap();
        assert_eq!(p.root.operator, "Nested loop inner join");
        // As MySQL 9 writes it: the tree under `query_plan`.
        let wrapped = r#"{"query": "select", "query_plan": {"operation": "Count rows in x",
            "table_name": "x", "actual_rows": 1.0, "actual_loops": 1}, "json_schema_version": "2.0"}"#;
        let w = parse(wrapped, ExplainMode::Analyze).unwrap();
        assert_eq!(w.root.operator, "Count rows in x");
        assert_eq!(w.root.actual_rows, Some(1.0));
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(p.root.children[0].actual_rows, Some(6.0));
        assert_eq!(p.root.children[0].target.as_deref(), Some("orders"));
    }

    #[test]
    fn nested_plans() {
        let raw = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "2.25"},
            "nested_loop": [
              {"table": {"table_name": "o", "access_type": "ALL", "rows_examined_per_scan": 10,
                         "attached_condition": "o.total > 10"}},
              {"table": {"table_name": "c", "access_type": "eq_ref", "key": "PRIMARY",
                         "rows": 1, "r_rows": 1, "r_loops": 3}}
            ]}}"#;
        let p = parse(raw, ExplainMode::Estimate).unwrap();
        assert_eq!(p.root.operator, "Query block");
        assert_eq!(p.root.cost, Some(2.25));
        let scan = &p.root.children[0];
        assert_eq!(scan.operator, "Full table scan");
        assert!(
            scan.properties
                .contains(&("attached_condition".into(), "o.total > 10".into()))
        );
        let lookup = &p.root.children[1];
        assert_eq!(lookup.target.as_deref(), Some("c using PRIMARY"));
        assert_eq!(lookup.actual_rows, Some(3.0));
        assert!(parse("{}", ExplainMode::Estimate).is_err());
    }
}
