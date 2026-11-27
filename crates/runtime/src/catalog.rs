//! What completion and the command palette know about a connection: its tables, views and
//! collections, read from the object tree once and kept until a statement changes the schema.

use crate::Runtime;
use std::collections::VecDeque;
use std::sync::Arc;
use treasury_core::config::ConnectionId;
use treasury_core::driver::Introspect;
use treasury_core::schema::{ChildQuery, ObjectKind, ObjectNode, ObjectPath};
use treasury_core::{Error, Result};
use treasury_ipc::{CatalogEntry, CatalogView, ColumnView};

/// The most objects kept; a schema with more is listed in part.
pub const MAX_OBJECTS: usize = 20_000;
/// The most tree nodes opened to find them.
const MAX_NODES: usize = 2_000;
const PAGE: u32 = 500;
/// The groups that hold tables, views and collections; the others (functions, sequences and so
/// on) are not opened.
const RELATION_GROUPS: &[&str] = &["Tables", "Views", "Materialized views", "Collections"];

fn entry(n: &ObjectNode) -> CatalogEntry {
    let find = |kinds: &[ObjectKind]| {
        kinds
            .iter()
            .find_map(|k| n.path.find(*k))
            .map(str::to_owned)
    };
    CatalogEntry {
        path: n.path.clone(),
        kind: n.kind,
        name: n.name.clone(),
        schema: find(&[ObjectKind::Schema, ObjectKind::Keyspace]),
        database: find(&[ObjectKind::Database, ObjectKind::Catalog]),
    }
}

/// Reads the tables, views and collections under the root of `tree`, breadth first.
pub async fn read(tree: &dyn Introspect) -> CatalogView {
    let mut entries = Vec::new();
    let mut queue = VecDeque::from([ObjectPath::root()]);
    let mut opened = 0;
    let mut complete = true;
    while let Some(path) = queue.pop_front() {
        if opened >= MAX_NODES || entries.len() >= MAX_OBJECTS {
            complete = false;
            break;
        }
        opened += 1;
        let mut cursor = None;
        loop {
            let query = ChildQuery {
                filter: None,
                cursor: cursor.take(),
                limit: PAGE,
            };
            let Ok(page) = tree.children(&path, &query).await else {
                complete = false;
                break;
            };
            for n in &page.nodes {
                match n.kind {
                    ObjectKind::Table
                    | ObjectKind::View
                    | ObjectKind::MaterializedView
                    | ObjectKind::Collection => entries.push(entry(n)),
                    ObjectKind::Database
                    | ObjectKind::Catalog
                    | ObjectKind::Schema
                    | ObjectKind::Keyspace
                        if n.expandable =>
                    {
                        queue.push_back(n.path.clone());
                    }
                    ObjectKind::Group
                        if n.expandable && RELATION_GROUPS.contains(&n.name.as_str()) =>
                    {
                        queue.push_back(n.path.clone());
                    }
                    _ => {}
                }
            }
            match page.next_cursor {
                Some(next) if entries.len() < MAX_OBJECTS => cursor = Some(next),
                Some(_) => {
                    complete = false;
                    break;
                }
                None => break,
            }
        }
    }
    entries.truncate(MAX_OBJECTS);
    CatalogView { entries, complete }
}

impl Runtime {
    /// The tables, views and collections of connection `id`, read on first use.
    pub async fn catalog(&self, id: &ConnectionId) -> Result<Arc<CatalogView>> {
        let live = self.live(id).await?;
        let mut cached = live.catalog.lock().await;
        if let Some(c) = cached.as_ref() {
            return Ok(c.clone());
        }
        let tree = live
            .meta
            .introspect()
            .ok_or_else(|| Error::unsupported("This connection has no object tree."))?;
        let catalog = Arc::new(read(tree).await);
        *cached = Some(catalog.clone());
        Ok(catalog)
    }

    /// The columns of the table or view at `path`.
    pub async fn columns(&self, id: &ConnectionId, path: &ObjectPath) -> Result<Vec<ColumnView>> {
        let live = self.live(id).await?;
        let tree = live
            .meta
            .introspect()
            .ok_or_else(|| Error::unsupported("This connection has no object tree."))?;
        Ok(tree
            .details(path)
            .await?
            .columns
            .into_iter()
            .map(|c| ColumnView {
                name: c.name,
                type_name: c.type_name,
            })
            .collect())
    }
}
