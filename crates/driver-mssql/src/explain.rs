//! Plans from showplan XML: `SET SHOWPLAN_XML` estimates without running the statement;
//! `SET STATISTICS XML` runs it and adds the actual counts.

use crate::session::{MsSqlSession, cell};
use async_trait::async_trait;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use treasury_core::driver::Explain;
use treasury_core::plan::{ExplainMode, Plan, PlanNode};
use treasury_core::{Error, Result};

fn attrs(e: &BytesStart<'_>) -> Vec<(String, String)> {
    e.attributes()
        .flatten()
        .map(|a| {
            let v = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_default();
            (a.key.as_ref().to_owned(), v)
        })
        .collect()
}

fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn strip(name: &str) -> &str {
    name.trim_matches(['[', ']'])
}

/// The tree of `RelOp` elements in a showplan document.
pub(crate) fn parse(xml: &str, mode: ExplainMode) -> Result<Plan> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<PlanNode> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::protocol(format!("The plan could not be read: {e}")))?;
        let (start, empty) = match &event {
            Event::Start(e) => (Some(e), false),
            Event::Empty(e) => (Some(e), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let name = e.name().as_ref().to_owned();
            let a = attrs(e);
            match name.as_str() {
                "RelOp" => {
                    let mut node = PlanNode {
                        operator: get(&a, "PhysicalOp").unwrap_or("?").to_owned(),
                        estimated_rows: get(&a, "EstimateRows").and_then(|v| v.parse().ok()),
                        cost: get(&a, "EstimatedTotalSubtreeCost").and_then(|v| v.parse().ok()),
                        ..PlanNode::default()
                    };
                    for key in [
                        "LogicalOp",
                        "EstimatedExecutionMode",
                        "Parallel",
                        "EstimateIO",
                        "EstimateCPU",
                    ] {
                        if let Some(v) = get(&a, key) {
                            node.properties.push((key.to_owned(), v.to_owned()));
                        }
                    }
                    if empty {
                        match stack.last_mut() {
                            Some(parent) => parent.children.push(node),
                            None => roots.push(node),
                        }
                    } else {
                        stack.push(node);
                    }
                }
                "Object" => {
                    if let Some(node) = stack.last_mut()
                        && node.target.is_none()
                    {
                        let table = get(&a, "Table").map(strip);
                        let index = get(&a, "Index").map(strip);
                        node.target = match (table, index) {
                            (Some(t), Some(i)) => Some(format!("{t} using {i}")),
                            (Some(t), None) => Some(t.to_owned()),
                            (None, Some(i)) => Some(i.to_owned()),
                            (None, None) => None,
                        };
                    }
                }
                "RunTimeCountersPerThread" => {
                    if let Some(node) = stack.last_mut() {
                        if let Some(rows) =
                            get(&a, "ActualRows").and_then(|v| v.parse::<f64>().ok())
                        {
                            node.actual_rows = Some(node.actual_rows.unwrap_or(0.0) + rows);
                        }
                        if let Some(ms) =
                            get(&a, "ActualElapsedms").and_then(|v| v.parse::<f64>().ok())
                        {
                            node.time_ms = Some(node.time_ms.unwrap_or(0.0).max(ms));
                        }
                    }
                }
                _ => {}
            }
        }
        match event {
            Event::End(e) if e.name().as_ref() == "RelOp" => {
                if let Some(node) = stack.pop() {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => roots.push(node),
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let root = match roots.len() {
        0 => return Err(Error::protocol("The plan has no operators.")),
        1 => roots.remove(0),
        _ => PlanNode {
            operator: "Batch".into(),
            children: roots,
            ..PlanNode::default()
        },
    };
    Ok(Plan {
        mode,
        root,
        raw: xml.to_owned(),
    })
}

#[async_trait]
impl Explain for MsSqlSession {
    async fn explain(&self, statement: &str, mode: ExplainMode) -> Result<Plan> {
        self.alive()?;
        if self.analyzer_statements(statement) > 1 {
            return Err(Error::unsupported("Plans are for one statement at a time."));
        }
        let (on, off) = match mode {
            ExplainMode::Estimate => ("SET SHOWPLAN_XML ON", "SET SHOWPLAN_XML OFF"),
            ExplainMode::Analyze => ("SET STATISTICS XML ON", "SET STATISTICS XML OFF"),
        };
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .ok_or_else(|| Error::protocol("The connection is closed."))?;
        // SET SHOWPLAN must be alone in its batch.
        MsSqlSession::batch(client, on)
            .await
            .map_err(|e| self.map(&e, None))?;
        let result = async {
            let sets = client.simple_query(statement).await?.into_results().await?;
            Ok::<_, tiberius::error::Error>(sets)
        }
        .await;
        let _ = MsSqlSession::batch(client, off).await;
        let sets = result.map_err(|e| self.map(&e, Some(statement)))?;
        // The plan is the last result set: one row, one XML column.
        let xml = sets
            .iter()
            .rev()
            .filter_map(|rows| rows.first().and_then(|r| cell(r, 0)))
            .find(|t| t.contains("ShowPlanXML"))
            .ok_or_else(|| Error::protocol("The server returned no plan."))?;
        parse(&xml, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan">
      <BatchSequence><Batch><Statements><StmtSimple>
        <QueryPlan>
          <RelOp PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" EstimatedTotalSubtreeCost="0.5">
            <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="3" ActualElapsedms="2"/></RunTimeInformation>
            <NestedLoops>
              <RelOp PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="10" EstimatedTotalSubtreeCost="0.1">
                <IndexScan><Object Database="[db]" Schema="[dbo]" Table="[orders]" Index="[PK_orders]"/></IndexScan>
              </RelOp>
              <RelOp PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.2">
                <RunTimeInformation>
                  <RunTimeCountersPerThread Thread="1" ActualRows="2"/>
                  <RunTimeCountersPerThread Thread="2" ActualRows="1"/>
                </RunTimeInformation>
                <IndexScan><Object Table="[customers]" Index="[PK_customers]"/></IndexScan>
              </RelOp>
            </NestedLoops>
          </RelOp>
        </QueryPlan>
      </StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;

    #[test]
    fn showplan_becomes_a_tree() {
        let p = parse(PLAN, ExplainMode::Analyze).unwrap();
        assert_eq!(p.root.operator, "Nested Loops");
        assert_eq!(p.root.actual_rows, Some(3.0));
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(
            p.root.children[0].target.as_deref(),
            Some("orders using PK_orders")
        );
        // Rows from every thread add up.
        assert_eq!(p.root.children[1].actual_rows, Some(3.0));
        assert!(parse("<ShowPlanXML/>", ExplainMode::Estimate).is_err());
    }
}
