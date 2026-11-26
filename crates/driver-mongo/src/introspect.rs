//! Databases, collections, views and indexes; details from a sample of documents.

use crate::session::MongoSession;
use crate::value::{convert, kind_of};
use async_trait::async_trait;
use mongodb::bson::{Bson, Document, doc};
use std::collections::BTreeMap;
use treasury_core::driver::Introspect;
use treasury_core::exec::ValueKind;
use treasury_core::schema::{
    ChildPage, ChildQuery, ColumnInfo, IndexInfo, ObjectDetails, ObjectKind, ObjectNode,
    ObjectPath, PathPart,
};
use treasury_core::{Error, ErrorKind, Result};

/// Documents sampled to describe a collection's fields.
const SAMPLE: i64 = 200;

/// One page of `names`, sorted, filtered by a case-insensitive substring, after `cursor`.
fn page(mut names: Vec<String>, q: &ChildQuery) -> (Vec<String>, Option<String>) {
    names.sort();
    let filter = q
        .filter
        .as_deref()
        .map(str::to_lowercase)
        .filter(|f| !f.is_empty());
    let limit = if q.limit == 0 {
        200
    } else {
        q.limit.min(2_000)
    } as usize;
    let mut out: Vec<String> = names
        .into_iter()
        .filter(|n| filter.as_ref().is_none_or(|f| n.to_lowercase().contains(f)))
        .filter(|n| q.cursor.as_ref().is_none_or(|c| n > c))
        .take(limit + 1)
        .collect();
    let next = (out.len() > limit).then(|| {
        out.truncate(limit);
        out.last().cloned().unwrap_or_default()
    });
    (out, next)
}

fn nodes(
    parent: &ObjectPath,
    kind: ObjectKind,
    names: Vec<String>,
    expandable: bool,
) -> Vec<ObjectNode> {
    names
        .into_iter()
        .map(|name| ObjectNode {
            path: parent.child(kind, &name),
            kind,
            expandable,
            detail: None,
            name,
        })
        .collect()
}

fn groups(parent: &ObjectPath, names: &[&str]) -> ChildPage {
    ChildPage {
        nodes: names
            .iter()
            .map(|g| ObjectNode {
                path: parent.child(ObjectKind::Group, *g),
                kind: ObjectKind::Group,
                name: (*g).to_owned(),
                expandable: true,
                detail: None,
            })
            .collect(),
        next_cursor: None,
    }
}

fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::Timestamp(_) => "timestamp",
        _ => "other",
    }
}

impl MongoSession {
    async fn collections(&self, db: &str, kind: &str) -> Result<Vec<String>> {
        let docs = self
            .meta_cursor(
                db,
                doc! {"listCollections": 1, "filter": {"type": kind}, "nameOnly": true, "authorizedCollections": true},
                100_000,
            )
            .await?;
        Ok(docs
            .iter()
            .filter_map(|d| d.get_str("name").ok().map(str::to_owned))
            .filter(|n| !n.starts_with("system."))
            .collect())
    }

    async fn indexes(&self, db: &str, coll: &str) -> Result<Vec<Document>> {
        self.meta_cursor(db, doc! {"listIndexes": coll}, 10_000)
            .await
    }

    async fn sampled_fields(&self, db: &str, coll: &str) -> Result<Vec<ColumnInfo>> {
        let docs = self
            .meta_cursor(
                db,
                doc! {"aggregate": coll, "pipeline": [{"$sample": {"size": SAMPLE}}], "cursor": {}},
                usize::try_from(SAMPLE).unwrap_or(200),
            )
            .await?;
        // Field -> (type name -> count), in first-seen order.
        let mut order: Vec<String> = Vec::new();
        let mut seen: BTreeMap<String, (BTreeMap<&'static str, usize>, ValueKind)> =
            BTreeMap::new();
        for d in &docs {
            for (k, v) in d {
                if !seen.contains_key(k) {
                    order.push(k.clone());
                }
                let entry = seen
                    .entry(k.clone())
                    .or_insert_with(|| (BTreeMap::new(), ValueKind::Any));
                *entry.0.entry(type_name(v)).or_insert(0) += 1;
                if entry.1 == ValueKind::Any && !matches!(v, Bson::Null) {
                    entry.1 = kind_of(&convert(v.clone()));
                }
            }
        }
        let total = docs.len().max(1);
        Ok(order
            .into_iter()
            .map(|name| {
                let (types, kind) = seen
                    .remove(&name)
                    .unwrap_or((BTreeMap::new(), ValueKind::Any));
                let present: usize = types.values().sum();
                let type_list = types
                    .iter()
                    .map(|(t, n)| {
                        if types.len() > 1 {
                            format!("{t} {}%", n * 100 / present.max(1))
                        } else {
                            (*t).to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                ColumnInfo {
                    primary_key: name == "_id",
                    nullable: present < total || types.contains_key("null"),
                    comment: (present < total)
                        .then(|| format!("in {}% of sampled documents", present * 100 / total)),
                    default: None,
                    kind,
                    type_name: type_list,
                    name,
                }
            })
            .collect())
    }
}

#[async_trait]
impl Introspect for MongoSession {
    async fn children(&self, parent: &ObjectPath, query: &ChildQuery) -> Result<ChildPage> {
        let db = parent.find(ObjectKind::Database);
        match parent.0.as_slice() {
            [] => {
                let list = self
                    .meta(
                        "admin",
                        doc! {"listDatabases": 1, "nameOnly": true, "authorizedDatabases": true},
                    )
                    .await?;
                let names = list
                    .get_array("databases")
                    .map(|a| {
                        a.iter()
                            .filter_map(Bson::as_document)
                            .filter_map(|d| d.get_str("name").ok().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let (names, next_cursor) = page(names, query);
                Ok(ChildPage {
                    nodes: nodes(parent, ObjectKind::Database, names, true),
                    next_cursor,
                })
            }
            [
                ..,
                PathPart {
                    kind: ObjectKind::Database,
                    ..
                },
            ] => Ok(groups(parent, &["Collections", "Views"])),
            [
                ..,
                owner,
                PathPart {
                    kind: ObjectKind::Group,
                    name,
                },
            ] => {
                let db = db.unwrap_or("test");
                let (names, kind, expandable) = match (owner.kind, name.as_str()) {
                    (ObjectKind::Database, "Collections") => (
                        self.collections(db, "collection").await?,
                        ObjectKind::Collection,
                        true,
                    ),
                    (ObjectKind::Database, "Views") => {
                        (self.collections(db, "view").await?, ObjectKind::View, false)
                    }
                    (ObjectKind::Collection, "Indexes") => (
                        self.indexes(db, &owner.name)
                            .await?
                            .iter()
                            .filter_map(|d| d.get_str("name").ok().map(str::to_owned))
                            .collect(),
                        ObjectKind::Index,
                        false,
                    ),
                    _ => return Ok(ChildPage::default()),
                };
                let (names, next_cursor) = page(names, query);
                Ok(ChildPage {
                    nodes: nodes(parent, kind, names, expandable),
                    next_cursor,
                })
            }
            [
                ..,
                PathPart {
                    kind: ObjectKind::Collection,
                    ..
                },
            ] => Ok(groups(parent, &["Indexes"])),
            _ => Ok(ChildPage::default()),
        }
    }

    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails> {
        let Some(leaf) = path.leaf() else {
            return Ok(ObjectDetails::default());
        };
        let db = path.find(ObjectKind::Database).unwrap_or("test");
        match leaf.kind {
            ObjectKind::Collection | ObjectKind::View => {
                let columns = self.sampled_fields(db, &leaf.name).await?;
                let indexes = if leaf.kind == ObjectKind::Collection {
                    self.indexes(db, &leaf.name)
                        .await?
                        .iter()
                        .map(|d| IndexInfo {
                            name: d.get_str("name").unwrap_or("").to_owned(),
                            columns: d
                                .get_document("key")
                                .map(|k| k.iter().map(|(f, dir)| format!("{f} {dir}")).collect())
                                .unwrap_or_default(),
                            unique: d.get_bool("unique").unwrap_or(false),
                            primary: d.get_str("name") == Ok("_id_"),
                            method: None,
                            predicate: d
                                .get_document("partialFilterExpression")
                                .ok()
                                .map(ToString::to_string),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let stats = if leaf.kind == ObjectKind::Collection {
                    self.meta_cursor(
                        db,
                        doc! {"aggregate": &leaf.name, "pipeline": [{"$collStats": {"storageStats": {}}}], "cursor": {}},
                        1,
                    )
                    .await
                    .ok()
                    .and_then(|v| v.into_iter().next())
                } else {
                    None
                };
                let storage = stats
                    .as_ref()
                    .and_then(|s| s.get_document("storageStats").ok());
                let num = |k: &str| {
                    storage.and_then(|s| match s.get(k) {
                        Some(Bson::Int32(n)) => u64::try_from(*n).ok(),
                        Some(Bson::Int64(n)) => u64::try_from(*n).ok(),
                        Some(Bson::Double(f)) => Some(*f as u64),
                        _ => None,
                    })
                };
                Ok(ObjectDetails {
                    columns,
                    indexes,
                    foreign_keys: Vec::new(),
                    row_estimate: num("count"),
                    size_bytes: num("storageSize"),
                    comment: None,
                    properties: vec![(
                        "Kind".to_owned(),
                        if leaf.kind == ObjectKind::View {
                            "view"
                        } else {
                            "collection"
                        }
                        .to_owned(),
                    )],
                })
            }
            ObjectKind::Database => {
                let stats = self.meta(db, doc! {"dbStats": 1}).await?;
                let n = |k: &str| match stats.get(k) {
                    Some(Bson::Int32(n)) => u64::try_from(*n).ok(),
                    Some(Bson::Int64(n)) => u64::try_from(*n).ok(),
                    Some(Bson::Double(f)) => Some(*f as u64),
                    _ => None,
                };
                Ok(ObjectDetails {
                    size_bytes: n("storageSize"),
                    properties: vec![
                        (
                            "Collections".to_owned(),
                            n("collections").unwrap_or(0).to_string(),
                        ),
                        ("Indexes".to_owned(), n("indexes").unwrap_or(0).to_string()),
                    ],
                    ..ObjectDetails::default()
                })
            }
            _ => Err(Error::new(
                ErrorKind::Unsupported,
                "No details for this object.",
            )),
        }
    }

    async fn ddl(&self, _path: &ObjectPath) -> Result<Option<String>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_filter_and_continue() {
        let names: Vec<String> = (0..25).map(|i| format!("c{i:02}")).collect();
        let q = ChildQuery {
            filter: Some("C1".into()),
            cursor: None,
            limit: 5,
        };
        let (first, next) = page(names.clone(), &q);
        assert_eq!(first, ["c10", "c11", "c12", "c13", "c14"]);
        let (rest, done) = page(names, &ChildQuery { cursor: next, ..q });
        assert_eq!(rest, ["c15", "c16", "c17", "c18", "c19"]);
        assert_eq!(done, None);
    }
}
