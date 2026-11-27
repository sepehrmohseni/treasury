//! Completion in the editor: the names of tables, views and columns of the open connection, and
//! common keywords, for the word being typed. The names come from the connection's catalog,
//! which the app reads from the object tree; columns are read for the tables the statement names.

use treasury_core::driver::SqlDialect;
use treasury_ipc::{CatalogEntry, ColumnView};
use treasury_sql::highlight::{SpanKind, highlight};
use treasury_sql::refs::{TableName, identifier, table_names};

/// The most suggestions shown at once.
const SHOWN: usize = 50;

/// Keywords suggested once two letters of one are typed.
const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "ON",
    "USING",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "AS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "UNION",
    "ALL",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "DROP",
    "ALTER",
    "ADD",
    "COLUMN",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "DEFAULT",
    "WITH",
    "RETURNING",
    "ASC",
    "DESC",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "CAST",
    "TRUE",
    "FALSE",
];

/// What completion would replace: the word before the caret, and the name before it when the
/// word follows a dot (`o.` or `shop.`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// Byte offset where the word starts.
    pub start: usize,
    /// The word so far.
    pub prefix: String,
    /// The name before the dot, without quotes.
    pub qualifier: Option<String>,
}

/// One suggestion: what the list shows, and what accepting it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    /// What it is: a column's type, a table's schema, "keyword".
    pub detail: String,
    pub insert: String,
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The completion context at byte offset `caret`, unless the caret is inside a string or a
/// comment, or not after a word or a dot.
pub fn context(dialect: SqlDialect, text: &str, caret: usize) -> Option<Context> {
    let before = text.get(..caret)?;
    let inside = highlight(dialect, text).into_iter().any(|s| {
        matches!(s.kind, SpanKind::String | SpanKind::Comment)
            && s.range.start < caret
            && caret <= s.range.end
    });
    if inside {
        return None;
    }
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| word_char(*c))
        .last()
        .map_or(caret, |(i, _)| i);
    let prefix = before[start..].to_owned();
    let qualifier = before[..start].strip_suffix('.').and_then(name_before);
    if prefix.is_empty() && qualifier.is_none() {
        return None;
    }
    Some(Context {
        start,
        prefix,
        qualifier,
    })
}

/// The name that ends `text`: a plain word, or one in quotes, backticks or brackets.
fn name_before(text: &str) -> Option<String> {
    let close = text.chars().last()?;
    let open = match close {
        '"' => '"',
        '`' => '`',
        ']' => '[',
        c if word_char(c) => {
            let start = text
                .char_indices()
                .rev()
                .take_while(|(_, c)| word_char(*c))
                .last()?
                .0;
            return Some(text[start..].to_owned());
        }
        _ => return None,
    };
    let body = &text[..text.len() - close.len_utf8()];
    let start = body.rfind(open)?;
    Some(body[start + open.len_utf8()..].to_owned())
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The catalog entry a name in the statement refers to.
pub fn resolve<'a>(t: &TableName, catalog: &'a [CatalogEntry]) -> Option<&'a CatalogEntry> {
    let qualified = |e: &&CatalogEntry| match t.qualifiers.last() {
        None => true,
        Some(q) => {
            e.schema.as_deref().is_some_and(|s| same(s, q))
                || e.database.as_deref().is_some_and(|d| same(d, q))
        }
    };
    catalog
        .iter()
        .filter(|e| same(&e.name, &t.name))
        .find(qualified)
}

/// The tables `statement` names that are in the catalog, for reading their columns.
pub fn tables_named<'a>(
    dialect: SqlDialect,
    statement: &str,
    catalog: &'a [CatalogEntry],
) -> Vec<&'a CatalogEntry> {
    table_names(dialect, statement)
        .iter()
        .filter_map(|t| resolve(t, catalog))
        .collect()
}

/// Suggestions for `ctx` in `statement`. `columns` gives a table's columns when they have been
/// read.
pub fn suggest(
    dialect: SqlDialect,
    ctx: &Context,
    statement: &str,
    catalog: &[CatalogEntry],
    columns: &dyn Fn(&CatalogEntry) -> Option<Vec<ColumnView>>,
) -> Vec<Suggestion> {
    let named = table_names(dialect, statement);
    let fits = |label: &str| {
        label.len() >= ctx.prefix.len()
            && label
                .get(..ctx.prefix.len())
                .is_some_and(|p| same(p, &ctx.prefix))
    };
    let column = |e: &CatalogEntry, c: ColumnView| Suggestion {
        insert: identifier(dialect, &c.name),
        detail: format!("{} · {}", c.type_name, e.name),
        label: c.name,
    };
    let table = |e: &CatalogEntry| Suggestion {
        label: e.name.clone(),
        detail: e
            .schema
            .clone()
            .or_else(|| e.database.clone())
            .unwrap_or_default(),
        insert: identifier(dialect, &e.name),
    };
    let mut out: Vec<Suggestion> = Vec::new();
    match &ctx.qualifier {
        Some(q) => {
            // `o.` for a table the statement names (or calls `o`): its columns.
            let target = named
                .iter()
                .find(|t| t.alias.as_deref().is_some_and(|a| same(a, q)) || same(&t.name, q))
                .and_then(|t| resolve(t, catalog));
            match target {
                Some(e) => out.extend(
                    columns(e)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| column(e, c)),
                ),
                // `shop.`: the tables of that schema or database.
                None => out.extend(
                    catalog
                        .iter()
                        .filter(|e| {
                            e.schema.as_deref().is_some_and(|s| same(s, q))
                                || e.database.as_deref().is_some_and(|d| same(d, q))
                        })
                        .map(table),
                ),
            }
        }
        None => {
            for t in &named {
                if let Some(e) = resolve(t, catalog) {
                    out.extend(
                        columns(e)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|c| column(e, c)),
                    );
                }
            }
            out.extend(catalog.iter().map(table));
            if ctx.prefix.chars().count() >= 2 {
                let lower = ctx.prefix.chars().all(|c| !c.is_uppercase());
                out.extend(KEYWORDS.iter().map(|k| Suggestion {
                    label: (*k).to_owned(),
                    detail: "keyword".into(),
                    insert: if lower {
                        k.to_lowercase()
                    } else {
                        (*k).to_owned()
                    },
                }));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| fits(&s.label) && !same(&s.label, &ctx.prefix) && seen.insert(s.insert.clone()));
    out.truncate(SHOWN);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use treasury_core::schema::{ObjectKind, ObjectPath};

    fn table(schema: &str, name: &str) -> CatalogEntry {
        let path = ObjectPath::root()
            .child(ObjectKind::Schema, schema)
            .child(ObjectKind::Group, "Tables")
            .child(ObjectKind::Table, name);
        CatalogEntry {
            path,
            kind: ObjectKind::Table,
            name: name.into(),
            schema: Some(schema.into()),
            database: None,
        }
    }

    fn catalog() -> Vec<CatalogEntry> {
        vec![
            table("public", "orders"),
            table("public", "order_lines"),
            table("audit", "Events"),
        ]
    }

    fn columns(e: &CatalogEntry) -> Option<Vec<ColumnView>> {
        let c = |n: &str, t: &str| ColumnView {
            name: n.into(),
            type_name: t.into(),
        };
        match e.name.as_str() {
            "orders" => Some(vec![
                c("id", "int8"),
                c("total", "numeric"),
                c("Status", "text"),
            ]),
            _ => None,
        }
    }

    fn labels(sql: &str) -> Vec<String> {
        let caret = sql.find('|').unwrap();
        let text = sql.replace('|', "");
        let Some(ctx) = context(SqlDialect::Postgres, &text, caret) else {
            return vec![];
        };
        suggest(SqlDialect::Postgres, &ctx, &text, &catalog(), &columns)
            .into_iter()
            .map(|s| s.label)
            .collect()
    }

    #[test]
    fn tables_after_from_and_columns_of_named_tables() {
        assert_eq!(
            labels("select * from ord|"),
            ["orders", "order_lines", "ORDER"]
        );
        assert_eq!(labels("select to| from orders"), ["total"]);
        assert_eq!(
            labels("select o.| from orders o"),
            ["id", "total", "Status"]
        );
        assert_eq!(labels("select * from audit.|"), ["Events"]);
    }

    #[test]
    fn nothing_inside_strings_or_comments() {
        assert!(labels("select 'ord|' from orders").is_empty());
        assert!(labels("select 1 -- ord|").is_empty());
    }

    #[test]
    fn what_is_written_is_quoted_when_it_must_be() {
        let text = "select o.st from orders o";
        let ctx = context(SqlDialect::Postgres, text, text.find(" from").unwrap()).unwrap();
        let got = suggest(SqlDialect::Postgres, &ctx, text, &catalog(), &columns);
        assert_eq!(got[0].insert, "\"Status\"");
        assert_eq!(ctx.qualifier.as_deref(), Some("o"));
        // Keywords follow the case typed.
        let text = "sel";
        let ctx = context(SqlDialect::Postgres, text, 3).unwrap();
        let got = suggest(SqlDialect::Postgres, &ctx, text, &[], &columns);
        assert_eq!(got[0].insert, "select");
    }

    #[test]
    fn a_quoted_qualifier_is_read_without_its_quotes() {
        assert_eq!(name_before("\"audit\""), Some("audit".into()));
        assert_eq!(name_before("x.[dbo]"), Some("dbo".into()));
        assert_eq!(name_before("a + b"), Some("b".into()));
    }
}

/// Fetching what completion needs, once per connection and table.
pub mod fetch {
    use crate::ipc;
    use crate::state::ui;
    use leptos::prelude::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use treasury_core::config::ConnectionId;
    use treasury_core::schema::ObjectPath;
    use treasury_ipc::{CatalogEntry, CatalogView, ColumnView};

    thread_local! {
        static CATALOGS: RefCell<HashSet<ConnectionId>> = RefCell::new(HashSet::new());
        static COLUMNS: RefCell<HashSet<(ConnectionId, ObjectPath)>> = RefCell::new(HashSet::new());
    }

    /// Reads the catalog of `connection`, unless it is read or being read.
    pub fn catalog(connection: &ConnectionId) {
        let state = ui();
        if state
            .catalogs
            .with_untracked(|m| m.contains_key(connection))
            || !CATALOGS.with(|s| s.borrow_mut().insert(connection.clone()))
        {
            return;
        }
        let id = connection.clone();
        leptos::task::spawn_local(async move {
            let reply =
                ipc::call::<_, CatalogView>("catalog", &serde_json::json!({ "id": id })).await;
            CATALOGS.with(|s| s.borrow_mut().remove(&id));
            // Without a catalog, completion offers keywords only; nothing to report.
            if let Ok(c) = reply {
                state.catalogs.update(|m| {
                    m.insert(id, c);
                });
            }
        });
    }

    /// Reads the columns of `table`, unless they are read or being read.
    pub fn columns(connection: &ConnectionId, table: &CatalogEntry) {
        let state = ui();
        let key = (connection.clone(), table.path.clone());
        if state.table_columns.with_untracked(|m| m.contains_key(&key))
            || !COLUMNS.with(|s| s.borrow_mut().insert(key.clone()))
        {
            return;
        }
        leptos::task::spawn_local(async move {
            let reply = ipc::call::<_, Vec<ColumnView>>(
                "columns",
                &serde_json::json!({ "id": key.0, "path": key.1 }),
            )
            .await;
            COLUMNS.with(|s| s.borrow_mut().remove(&key));
            if let Ok(c) = reply {
                state.table_columns.update(|m| {
                    m.insert(key, c);
                });
            }
        });
    }

    /// Forgets what was read of `connection`, after a statement that may change its schema.
    pub fn forget(connection: &ConnectionId) {
        let state = ui();
        state.catalogs.update(|m| {
            m.remove(connection);
        });
        state
            .table_columns
            .update(|m| m.retain(|(c, _), _| c != connection));
    }
}
