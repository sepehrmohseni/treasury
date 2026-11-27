//! The MongoDB editor language: database commands written as (Extended) JSON documents, one or
//! more per script, such as `{"find": "orders", "filter": {"status": "open"}}`.
//!
//! Classification uses a table of command names. Aggregations with `$out` or `$merge` write.
//! Commands Treasury does not know are `Unknown`, which the guardrails treat as writes.

use serde_json::Value as Json;
use treasury_core::analyze::{
    Analyzer, Classified, CountPlan, Destructive, EditSource, Flags, LimitPlan, Placeholder,
    Projected, StatementClass,
};
use treasury_core::edit::TableRef;

/// The MongoDB analyzer.
#[derive(Default, Clone, Copy)]
pub struct MongoAnalyzer;

/// One command of a script: its byte range and the parsed document, when it parses.
pub(crate) struct Piece {
    pub range: std::ops::Range<usize>,
    pub doc: Result<serde_json::Map<String, Json>, String>,
}

/// Splits a script into its top-level JSON values.
pub(crate) fn pieces(script: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut stream = serde_json::Deserializer::from_str(script).into_iter::<Json>();
    let mut start = skip_space(script, 0);
    loop {
        match stream.next() {
            None => break,
            Some(Ok(v)) => {
                let end = stream.byte_offset();
                out.push(Piece {
                    range: start..end,
                    doc: match v {
                        Json::Object(m) => Ok(m),
                        _ => Err(
                            "A command is a JSON object, such as {\"find\": \"orders\"}.".into(),
                        ),
                    },
                });
                start = skip_space(script, end);
            }
            Some(Err(e)) => {
                let end = script.trim_end().len().max(start);
                out.push(Piece {
                    range: start..end,
                    doc: Err(e.to_string()),
                });
                break;
            }
        }
    }
    out
}

fn skip_space(s: &str, from: usize) -> usize {
    s.get(from..)
        .map_or(from, |rest| from + (rest.len() - rest.trim_start().len()))
}

/// The command name: the first key, which MongoDB treats as the command.
pub(crate) fn command_name(doc: &serde_json::Map<String, Json>) -> Option<&str> {
    doc.keys().next().map(String::as_str)
}

fn pipeline_writes(doc: &serde_json::Map<String, Json>) -> bool {
    doc.get("pipeline")
        .and_then(Json::as_array)
        .is_some_and(|stages| {
            stages.iter().any(|s| {
                s.as_object()
                    .is_some_and(|o| o.contains_key("$out") || o.contains_key("$merge"))
            })
        })
}

fn empty_filter(v: Option<&Json>) -> bool {
    v.is_none_or(|q| q.as_object().is_some_and(serde_json::Map::is_empty))
}

/// Class and flags of one command.
pub(crate) fn verdict(doc: &serde_json::Map<String, Json>) -> (StatementClass, Flags) {
    use StatementClass as C;
    let mut flags = Flags::default();
    let Some(name) = command_name(doc) else {
        return (C::Unknown, flags);
    };
    let class = match name.to_ascii_lowercase().as_str() {
        "find" | "count" | "distinct" | "listcollections" | "listindexes" | "listdatabases"
        | "dbstats" | "collstats" | "datasize" | "serverstatus" | "hello" | "ismaster" | "ping"
        | "buildinfo" | "hostinfo" | "connectionstatus" | "getmore" | "getparameter"
        | "replsetgetstatus" | "currentop" | "top" | "validate" | "usersinfo" | "rolesinfo"
        | "listsearchindexes" | "getlog" => C::Read,
        "aggregate" => {
            if pipeline_writes(doc) {
                C::Write
            } else {
                C::Read
            }
        }
        // Explaining never writes, though "executionStats" runs the read it explains.
        "explain" => {
            flags.executes = doc
                .get("verbosity")
                .and_then(Json::as_str)
                .is_some_and(|v| v != "queryPlanner");
            C::Read
        }
        "insert" => C::Write,
        "update" => {
            flags.unfiltered = doc
                .get("updates")
                .and_then(Json::as_array)
                .is_some_and(|u| u.iter().any(|e| empty_filter(e.get("q"))));
            C::Write
        }
        "delete" => {
            flags.unfiltered = doc
                .get("deletes")
                .and_then(Json::as_array)
                .is_some_and(|d| d.iter().any(|e| empty_filter(e.get("q"))));
            C::Write
        }
        "findandmodify" | "bulkwrite" => C::Write,
        "create" | "createindexes" | "createview" => C::Ddl,
        "drop" | "dropdatabase" | "dropindexes" => {
            flags.destructive = Some(Destructive::Drop);
            C::Ddl
        }
        "collmod" | "renamecollection" | "converttocapped" | "compact" => {
            flags.destructive = Some(Destructive::Alter);
            C::Ddl
        }
        "killop"
        | "killcursors"
        | "killsessions"
        | "createuser"
        | "updateuser"
        | "dropuser"
        | "dropallusersfromdatabase"
        | "grantrolestouser"
        | "revokerolesfromuser"
        | "createrole"
        | "droprole"
        | "setparameter"
        | "fsync"
        | "fsyncunlock"
        | "logrotate"
        | "replsetstepdown"
        | "replsetfreeze"
        | "replsetreconfig"
        | "replsetinitiate" => C::Admin,
        "shutdown" => {
            flags.production_forbidden = true;
            C::Admin
        }
        _ => C::Unknown,
    };
    (class, flags)
}

fn summary(doc: &serde_json::Map<String, Json>) -> String {
    match doc.iter().next() {
        Some((name, Json::String(target))) => format!("{name} {target}"),
        Some((name, _)) => name.clone(),
        None => "{}".into(),
    }
}

/// Inserts `addition` before the closing brace of the command object in `text`.
fn insert_before_close(text: &str, addition: &str) -> Option<(String, usize)> {
    let close = text.trim_end().rfind('}')?;
    let mut out = String::with_capacity(text.len() + addition.len());
    out.push_str(&text[..close]);
    out.push_str(addition);
    out.push_str(&text[close..]);
    Some((out, close))
}

impl Analyzer for MongoAnalyzer {
    fn classify(&self, script: &str) -> Vec<Classified> {
        pieces(script)
            .into_iter()
            .map(|p| match &p.doc {
                Ok(doc) => {
                    let (class, flags) = verdict(doc);
                    Classified {
                        range: p.range,
                        class,
                        flags,
                        summary: summary(doc),
                        reason: (class == StatementClass::Unknown).then(|| {
                            format!(
                                "Treasury does not know the command {}.",
                                command_name(doc).unwrap_or("{}")
                            )
                        }),
                    }
                }
                Err(e) => Classified {
                    range: p.range,
                    class: StatementClass::Unknown,
                    flags: Flags::default(),
                    summary: String::new(),
                    reason: Some(e.clone()),
                },
            })
            .collect()
    }

    fn limit(&self, statement: &str, limit: u64) -> LimitPlan {
        let Some(Piece { doc: Ok(doc), .. }) = pieces(statement).into_iter().next() else {
            return LimitPlan::NotApplicable;
        };
        match command_name(&doc) {
            Some("find") if !doc.contains_key("limit") => {
                let addition = format!(", \"limit\": {limit}");
                match insert_before_close(statement, &addition) {
                    Some((text, at)) => LimitPlan::Rewritten {
                        text,
                        inserted_at: at,
                        inserted_len: addition.len(),
                    },
                    None => LimitPlan::StopFetching,
                }
            }
            Some("find" | "aggregate") => LimitPlan::StopFetching,
            _ => LimitPlan::NotApplicable,
        }
    }

    fn count(&self, statement: &str) -> CountPlan {
        let all = pieces(statement);
        let [Piece { doc: Ok(doc), .. }] = all.as_slice() else {
            return CountPlan::Underivable {
                reason: "Treasury cannot preview how many documents it changes: it is not a \
                         single command."
                    .into(),
            };
        };
        let (list_key, multi_default) = match command_name(doc) {
            Some("update") => ("updates", false),
            Some("delete") => ("deletes", true),
            _ => return CountPlan::NotApplicable,
        };
        let entries = doc.get(list_key).and_then(Json::as_array);
        let (Some(collection), Some([entry])) = (
            command_name(doc)
                .and_then(|n| doc.get(n))
                .and_then(Json::as_str),
            entries.map(Vec::as_slice),
        ) else {
            return CountPlan::Underivable {
                reason: "Treasury cannot preview how many documents it changes: it holds \
                         several changes."
                    .into(),
            };
        };
        let filter = entry
            .get("q")
            .cloned()
            .unwrap_or(Json::Object(Default::default()));
        // update changes one document unless multi; delete with limit 1 removes one.
        let one = match list_key {
            "updates" => !entry
                .get("multi")
                .and_then(Json::as_bool)
                .unwrap_or(multi_default),
            _ => entry.get("limit").and_then(Json::as_i64) == Some(1),
        };
        let mut pipeline = vec![serde_json::json!({ "$match": filter })];
        if one {
            pipeline.push(serde_json::json!({ "$limit": 1 }));
        }
        pipeline.push(serde_json::json!({ "$count": "n" }));
        let query = serde_json::json!({
            "aggregate": collection,
            "pipeline": pipeline,
            "cursor": {},
        });
        CountPlan::Query(query.to_string())
    }

    /// The collection a `find` reads, so its documents can be edited in the grid. With a
    /// projection that names fields, those fields; with none, or one that only leaves fields out,
    /// every field. Aggregations make new documents, so they cannot be edited.
    fn edit_source(&self, statement: &str) -> Result<EditSource, String> {
        let pieces = pieces(statement);
        let [piece] = pieces.as_slice() else {
            return Err("Only the result of one find command can be edited.".into());
        };
        let doc = piece.doc.as_ref().map_err(Clone::clone)?;
        if command_name(doc) != Some("find") {
            return Err(
                "Only documents a find command returned can be edited; an aggregation makes new \
                 documents."
                    .into(),
            );
        }
        let collection = doc
            .get("find")
            .and_then(Json::as_str)
            .ok_or("The find command names no collection.")?;
        let included: Vec<(&String, &Json)> = doc
            .get("projection")
            .and_then(Json::as_object)
            .map(|p| {
                p.iter()
                    .filter(|(_, v)| !matches!(v, Json::Bool(false)) && v.as_i64() != Some(0))
                    .collect()
            })
            .unwrap_or_default();
        let items = if included.is_empty() {
            vec![Projected::AllColumns]
        } else {
            included
                .into_iter()
                .map(|(field, v)| {
                    // `{"total": 1}` shows the field; `{"total": {"$sum": ...}}` computes one.
                    if matches!(v, Json::Bool(true)) || v.as_i64() == Some(1) {
                        Projected::Column(field.clone())
                    } else {
                        Projected::Expression
                    }
                })
                .chain(std::iter::once(Projected::Column("_id".into())))
                .collect()
        };
        Ok(EditSource {
            table: TableRef {
                schema: doc.get("$db").and_then(Json::as_str).map(str::to_owned),
                table: collection.to_owned(),
            },
            items,
        })
    }

    fn placeholders(&self, _statement: &str) -> Vec<Placeholder> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(s: &str) -> (StatementClass, Flags) {
        let c = MongoAnalyzer.classify(s);
        assert_eq!(c.len(), 1, "{s}");
        (c[0].class, c[0].flags.clone())
    }

    #[test]
    fn commands_by_name() {
        assert_eq!(
            class(r#"{"find": "orders", "filter": {}}"#).0,
            StatementClass::Read
        );
        assert_eq!(
            class(r#"{"aggregate": "o", "pipeline": [{"$match": {}}], "cursor": {}}"#).0,
            StatementClass::Read
        );
        assert_eq!(
            class(r#"{"aggregate": "o", "pipeline": [{"$out": "x"}], "cursor": {}}"#).0,
            StatementClass::Write
        );
        assert_eq!(
            class(r#"{"aggregate": "o", "pipeline": [{"$merge": {"into": "x"}}]}"#).0,
            StatementClass::Write
        );
        assert_eq!(
            class(r#"{"insert": "o", "documents": [{}]}"#).0,
            StatementClass::Write
        );
        let (c, f) = class(r#"{"drop": "orders"}"#);
        assert_eq!(c, StatementClass::Ddl);
        assert_eq!(f.destructive, Some(Destructive::Drop));
        assert!(class(r#"{"shutdown": 1}"#).1.production_forbidden);
        assert_eq!(class(r#"{"frobnicate": 1}"#).0, StatementClass::Unknown);
        assert_eq!(class("{not json").0, StatementClass::Unknown);
        assert_eq!(class("[1, 2]").0, StatementClass::Unknown);
    }

    #[test]
    fn empty_filters_are_unfiltered() {
        assert!(
            class(r#"{"delete": "o", "deletes": [{"q": {}, "limit": 0}]}"#)
                .1
                .unfiltered
        );
        assert!(
            class(r#"{"update": "o", "updates": [{"u": {"$set": {"a": 1}}}]}"#)
                .1
                .unfiltered
        );
        assert!(
            !class(r#"{"update": "o", "updates": [{"q": {"a": 1}, "u": {}}]}"#)
                .1
                .unfiltered
        );
    }

    #[test]
    fn scripts_split_into_commands() {
        let s = "{\"find\": \"a\"}\n  {\"drop\": \"b\"}  ";
        let c = MongoAnalyzer.classify(s);
        assert_eq!(c.len(), 2);
        assert_eq!(&s[c[1].range.clone()], "{\"drop\": \"b\"}");
    }

    #[test]
    fn find_gets_a_limit_and_aggregate_stops_reading() {
        match MongoAnalyzer.limit(r#"{"find": "o", "filter": {}}"#, 50) {
            LimitPlan::Rewritten { text, .. } => {
                assert_eq!(text, r#"{"find": "o", "filter": {}, "limit": 50}"#);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            MongoAnalyzer.limit(r#"{"find": "o", "limit": 5}"#, 50),
            LimitPlan::StopFetching
        );
        assert_eq!(
            MongoAnalyzer.limit(r#"{"aggregate": "o", "pipeline": []}"#, 50),
            LimitPlan::StopFetching
        );
        assert_eq!(
            MongoAnalyzer.limit(r#"{"ping": 1}"#, 50),
            LimitPlan::NotApplicable
        );
    }

    #[test]
    fn found_documents_can_be_edited() {
        let a = MongoAnalyzer;
        let s = a
            .edit_source(r#"{"find": "users", "filter": {}, "$db": "app"}"#)
            .unwrap();
        assert_eq!(
            (s.table.schema.as_deref(), s.table.table.as_str()),
            (Some("app"), "users")
        );
        assert_eq!(s.items, vec![Projected::AllColumns]);
        let s = a
            .edit_source(r#"{"find": "users", "projection": {"name": 1, "secret": 0, "n": {"$size": "$tags"}}}"#)
            .unwrap();
        assert_eq!(
            s.items,
            vec![
                Projected::Column("name".into()),
                Projected::Expression,
                Projected::Column("_id".into())
            ]
        );
        // Leaving fields out still shows the others as they are.
        let s = a
            .edit_source(r#"{"find": "users", "projection": {"secret": 0}}"#)
            .unwrap();
        assert_eq!(s.items, vec![Projected::AllColumns]);
        assert!(
            a.edit_source(r#"{"aggregate": "users", "pipeline": []}"#)
                .is_err()
        );
        assert!(a.edit_source(r#"{"find": "a"} {"find": "b"}"#).is_err());
    }

    #[test]
    fn previews_count_the_same_filter() {
        let CountPlan::Query(q) = MongoAnalyzer
            .count(r#"{"delete": "orders", "deletes": [{"q": {"status": "old"}, "limit": 0}]}"#)
        else {
            panic!()
        };
        let v: Json = serde_json::from_str(&q).unwrap();
        assert_eq!(v["aggregate"], "orders");
        assert_eq!(v["pipeline"][0]["$match"]["status"], "old");
        assert_eq!(v["pipeline"][1]["$count"], "n");
        let CountPlan::Query(one) = MongoAnalyzer.count(
            r#"{"update": "orders", "updates": [{"q": {"a": 1}, "u": {"$set": {"b": 2}}}]}"#,
        ) else {
            panic!()
        };
        assert!(one.contains("\"$limit\":1"));
        assert!(matches!(
            MongoAnalyzer.count(r#"{"delete": "o", "deletes": [{"q": {}}, {"q": {}}]}"#),
            CountPlan::Underivable { .. }
        ));
        assert_eq!(
            MongoAnalyzer.count(r#"{"find": "o"}"#),
            CountPlan::NotApplicable
        );
    }
}
