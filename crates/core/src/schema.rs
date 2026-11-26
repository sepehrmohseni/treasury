//! The object tree and object details that introspection produces.

use crate::exec::ValueKind;
use serde::{Deserialize, Serialize};

/// Kinds of objects in the tree, across engines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// A group node such as "Tables" or "Views"; its name says which.
    Group,
    /// Database (or Snowflake database, Redis database number).
    Database,
    /// Unity Catalog catalog.
    Catalog,
    /// Schema.
    Schema,
    /// Cassandra keyspace.
    Keyspace,
    /// Table.
    Table,
    /// View.
    View,
    /// Materialized view.
    MaterializedView,
    /// Function.
    Function,
    /// Stored procedure.
    Procedure,
    /// Package (Oracle).
    Package,
    /// Sequence.
    Sequence,
    /// Synonym (Oracle).
    Synonym,
    /// User-defined type.
    Type,
    /// Extension (PostgreSQL).
    Extension,
    /// Index.
    Index,
    /// Constraint.
    Constraint,
    /// Trigger.
    Trigger,
    /// Scheduled event (MySQL).
    Event,
    /// Column.
    Column,
    /// MongoDB collection.
    Collection,
    /// A Redis key prefix grouping keys by delimiter.
    KeyPrefix,
    /// A Redis key.
    Key,
    /// Snowflake stage.
    Stage,
    /// Snowflake or Databricks warehouse.
    Warehouse,
    /// Unity Catalog volume.
    Volume,
}

/// One step in an object's path from the root.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PathPart {
    /// The kind of the object at this step.
    pub kind: ObjectKind,
    /// Its name.
    pub name: String,
}

/// The path from the connection root to an object; empty for the root itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectPath(pub Vec<PathPart>);

impl ObjectPath {
    /// The connection root.
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// This path extended by one step.
    pub fn child(&self, kind: ObjectKind, name: impl Into<String>) -> Self {
        let mut parts = self.0.clone();
        parts.push(PathPart {
            kind,
            name: name.into(),
        });
        Self(parts)
    }

    /// The name of the nearest ancestor (or self) of `kind`.
    pub fn find(&self, kind: ObjectKind) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|p| p.kind == kind)
            .map(|p| p.name.as_str())
    }

    /// The last step.
    pub fn leaf(&self) -> Option<&PathPart> {
        self.0.last()
    }
}

/// A node in the object tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectNode {
    /// Where it is.
    pub path: ObjectPath,
    /// What it is.
    pub kind: ObjectKind,
    /// Its name as displayed.
    pub name: String,
    /// Whether it has children to load.
    pub expandable: bool,
    /// A short fact shown after the name, such as "1.2 M rows".
    pub detail: Option<String>,
}

/// What to load under a node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildQuery {
    /// Only children whose name contains this text, case-insensitively.
    pub filter: Option<String>,
    /// Continue after a previous page.
    pub cursor: Option<String>,
    /// At most this many nodes.
    pub limit: u32,
}

/// One page of children.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildPage {
    /// The nodes.
    pub nodes: Vec<ObjectNode>,
    /// Pass this as `cursor` for the next page; `None` when this was the last.
    pub next_cursor: Option<String>,
}

/// A column of a table or view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// Name.
    pub name: String,
    /// Type as in DDL.
    pub type_name: String,
    /// Logical kind.
    pub kind: ValueKind,
    /// NULL allowed.
    pub nullable: bool,
    /// Default expression.
    pub default: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Part of the primary key.
    pub primary_key: bool,
}

/// An index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexInfo {
    /// Name.
    pub name: String,
    /// Key columns or expressions, in order.
    pub columns: Vec<String>,
    /// Unique.
    pub unique: bool,
    /// Backs the primary key.
    pub primary: bool,
    /// Access method (btree, hash, gin, clustered...).
    pub method: Option<String>,
    /// Partial index predicate.
    pub predicate: Option<String>,
}

/// A foreign key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    /// Name.
    pub name: String,
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Referenced schema.
    pub ref_schema: Option<String>,
    /// Referenced table.
    pub ref_table: String,
    /// Referenced columns.
    pub ref_columns: Vec<String>,
}

/// Everything known about one object.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectDetails {
    /// Columns (tables, views, collections with a sampled schema).
    pub columns: Vec<ColumnInfo>,
    /// Indexes.
    pub indexes: Vec<IndexInfo>,
    /// Foreign keys.
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// Estimated rows or documents.
    pub row_estimate: Option<u64>,
    /// Size on disk, in bytes.
    pub size_bytes: Option<u64>,
    /// Comment.
    pub comment: Option<String>,
    /// Other facts as label and value (owner, engine, TTL, partition key...).
    pub properties: Vec<(String, String)>,
}

impl ObjectDetails {
    /// The primary key columns, in key order when the engine reports it.
    pub fn primary_key(&self) -> Vec<&str> {
        if let Some(pk) = self.indexes.iter().find(|i| i.primary) {
            return pk.columns.iter().map(String::as_str).collect();
        }
        self.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_find_ancestors() {
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "shop")
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "orders");
        assert_eq!(p.find(ObjectKind::Schema), Some("public"));
        assert_eq!(p.find(ObjectKind::Catalog), None);
        assert_eq!(p.leaf().map(|l| l.name.as_str()), Some("orders"));
    }

    #[test]
    fn primary_key_prefers_the_index_order() {
        let col = |name: &str, pk: bool| ColumnInfo {
            name: name.into(),
            type_name: "int".into(),
            kind: ValueKind::Integer,
            nullable: false,
            default: None,
            comment: None,
            primary_key: pk,
        };
        let mut d = ObjectDetails {
            columns: vec![col("a", true), col("b", true)],
            ..ObjectDetails::default()
        };
        assert_eq!(d.primary_key(), vec!["a", "b"]);
        d.indexes.push(IndexInfo {
            name: "pk".into(),
            columns: vec!["b".into(), "a".into()],
            unique: true,
            primary: true,
            method: None,
            predicate: None,
        });
        assert_eq!(d.primary_key(), vec!["b", "a"]);
    }
}
