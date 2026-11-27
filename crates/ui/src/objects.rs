//! The editor of a table, view, collection or other object: its properties (columns, indexes,
//! foreign keys, DDL and facts) and, for objects with rows, its data in the editable grid.

use crate::ipc;
use crate::navigator::{first_page, has_rows};
use crate::state::{ObjectPage, ui};
use crate::tabs;
use leptos::prelude::*;
use std::collections::{HashMap, HashSet};
use treasury_core::config::ConnectionId;
use treasury_core::schema::{ObjectDetails, ObjectKind, ObjectNode};

/// What was read about an object editor's object, by tab.
#[derive(Clone, Debug, Default, PartialEq)]
struct Info {
    details: Option<Result<ObjectDetails, String>>,
    ddl: Option<Result<Option<String>, String>>,
}

thread_local! {
    /// Reference-counted rather than owned, so no component or effect that reads it first can
    /// dispose of it.
    static INFO: ArcRwSignal<HashMap<String, Info>> = ArcRwSignal::new(HashMap::new());
    /// Object editor tabs whose rows have been asked for, so showing them again does not
    /// read them again; Refresh does.
    static READ: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());
}

fn info() -> ArcRwSignal<HashMap<String, Info>> {
    INFO.with(Clone::clone)
}

/// Reads the details and the DDL of tab `tab`'s object.
fn load(tab: String, connection: ConnectionId, node: ObjectNode) {
    let store = info();
    store.update(|m| {
        m.insert(tab.clone(), Info::default());
    });
    let args = serde_json::json!({ "id": connection, "path": node.path });
    {
        let (tab, args, store) = (tab.clone(), args.clone(), store.clone());
        leptos::task::spawn_local(async move {
            let got = ipc::call::<_, ObjectDetails>("details", &args)
                .await
                .map_err(|e| e.to_string());
            store.update(|m| m.entry(tab).or_default().details = Some(got));
        });
    }
    leptos::task::spawn_local(async move {
        let got = ipc::call::<_, Option<String>>("ddl", &args)
            .await
            .map_err(|e| e.to_string());
        store.update(|m| m.entry(tab).or_default().ddl = Some(got));
    });
}

/// Reads tab `tab`'s rows, the first time they are shown or again on Refresh.
fn read_rows(tab: &str, connection: &ConnectionId, node: &ObjectNode, again: bool) {
    let state = ui();
    let first = READ.with(|r| r.borrow_mut().insert(tab.to_owned()));
    if !first && !again {
        return;
    }
    let config = state.connections.with_untracked(|l| {
        l.iter()
            .find(|c| &c.config.id == connection)
            .map(|c| c.config.clone())
    });
    if let Some(statement) = config.and_then(|c| crate::editor::browse_statement(&c, node)) {
        state.notice.set(None);
        crate::editor::start(tab.to_owned(), statement, true);
    }
}

/// Forgets what was read for tab `tab`, once it is closed.
pub fn forget(tab: &str) {
    info().update(|m| {
        m.remove(tab);
    });
    READ.with(|r| r.borrow_mut().remove(tab));
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "Table",
        ObjectKind::View => "View",
        ObjectKind::MaterializedView => "Materialized view",
        ObjectKind::Collection => "Collection",
        ObjectKind::Function => "Function",
        ObjectKind::Procedure => "Procedure",
        ObjectKind::Sequence => "Sequence",
        ObjectKind::Type => "Type",
        ObjectKind::Trigger => "Trigger",
        ObjectKind::Event => "Event",
        ObjectKind::Index => "Index",
        ObjectKind::Constraint => "Constraint",
        ObjectKind::Extension => "Extension",
        ObjectKind::Package => "Package",
        ObjectKind::Synonym => "Synonym",
        _ => "Object",
    }
}

/// Where the object is, as "database › schema", without the tree's group folders.
fn location(node: &ObjectNode) -> String {
    let parts = &node.path.0;
    parts
        .iter()
        .take(parts.len().saturating_sub(1))
        .filter(|p| p.kind != ObjectKind::Group)
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

/// `n` bytes for reading: "512 B", "64.0 kB", "1.2 MB".
pub fn size_text(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The shown tab, when it is an object editor: its id, connection and object.
fn shown() -> Option<(String, ConnectionId, ObjectNode)> {
    let state = ui();
    let id = state.current()?;
    let tab = state.tab(&id)?;
    Some((id, tab.connection, tab.object?))
}

/// Above an object editor's pages: what the object is and where, and Properties and Data.
#[component]
pub fn ObjectHeader() -> impl IntoView {
    let state = ui();
    let current = Memo::new(move |_| shown());
    // A newly shown editor reads its object once; its rows are read when Data is first shown.
    Effect::new(move |_| {
        let Some((tab, connection, node)) = current.get() else {
            return;
        };
        if !info().with_untracked(|m| m.contains_key(&tab)) {
            load(tab.clone(), connection.clone(), node.clone());
        }
        if state.pages.with(|m| m.get(&tab) == Some(&ObjectPage::Data)) {
            read_rows(&tab, &connection, &node, false);
        }
    });
    move || {
        current.get().map(|(tab, connection, node)| {
            let page = {
                let (tab, kind) = (tab.clone(), node.kind);
                Memo::new(move |_| state.pages.with(|m| m.get(&tab).copied()).unwrap_or(first_page(kind)))
            };
            let facts = {
                let tab = tab.clone();
                move || {
                    info().with(|m| {
                        let d = m.get(&tab)?.details.as_ref()?.as_ref().ok()?;
                        let mut out = Vec::new();
                        if let Some(n) = d.row_estimate {
                            out.push(format!("about {} rows", crate::grid::thousands(n)));
                        }
                        if let Some(n) = d.size_bytes {
                            out.push(size_text(n));
                        }
                        (!out.is_empty()).then(|| out.join(" · "))
                    })
                }
            };
            let set_page = {
                let tab = tab.clone();
                move |p: ObjectPage| {
                    state.pages.update(|m| {
                        m.insert(tab.clone(), p);
                    });
                }
            };
            let to_props = {
                let set_page = set_page.clone();
                move |_| set_page(first_page(node.kind))
            };
            let to_data = set_page.clone();
            let refresh = {
                let (tab, connection, node) = (tab.clone(), connection.clone(), node.clone());
                move |_| {
                    if page.get_untracked() == ObjectPage::Data {
                        read_rows(&tab, &connection, &node, true);
                    } else {
                        load(tab.clone(), connection.clone(), node.clone());
                    }
                }
            };
            let place = location(&node);
            view! {
                <div class="object-head">
                    <div class="object-title">
                        <span class="kind">{kind_label(node.kind)}</span>
                        <strong class="object-name">{node.name.clone()}</strong>
                        {(!place.is_empty()).then(|| view! { <span class="where">{format!("in {place}")}</span> })}
                        <span class="facts">{facts}</span>
                    </div>
                    <div class="object-tabs" role="tablist" aria-label="Object editor">
                        <button role="tab" class="props-tab" class:on=move || page.get() != ObjectPage::Data
                            aria-selected=move || (page.get() != ObjectPage::Data).to_string()
                            on:click=to_props>"Properties"</button>
                        {has_rows(node.kind).then(|| view! {
                            <button role="tab" class="data-tab" class:on=move || page.get() == ObjectPage::Data
                                aria-selected=move || (page.get() == ObjectPage::Data).to_string()
                                on:click=move |_| to_data(ObjectPage::Data)>"Data"</button>
                        })}
                        <span class="push"></span>
                        <button class="btn small object-refresh" title="Read it again from the server" on:click=refresh>"Refresh"</button>
                    </div>
                </div>
            }
        })
    }
}

/// An object editor's properties: a list of sections and the one chosen.
#[component]
pub fn ObjectProps() -> impl IntoView {
    let state = ui();
    let current = Memo::new(move |_| shown());
    move || {
        let (tab, connection, node) = current.get()?;
        let page = {
            let tab = tab.clone();
            Memo::new(move |_| {
                state
                    .pages
                    .with(|m| m.get(&tab).copied())
                    .unwrap_or(first_page(node.kind))
            })
        };
        if page.get() == ObjectPage::Data {
            return None;
        }
        let mut sections = Vec::new();
        if has_rows(node.kind) {
            sections.extend([
                (ObjectPage::Columns, "Columns", "s-columns"),
                (ObjectPage::Indexes, "Indexes", "s-indexes"),
                (ObjectPage::ForeignKeys, "Foreign keys", "s-foreign-keys"),
            ]);
        }
        sections.extend([
            (ObjectPage::Ddl, "DDL", "s-ddl"),
            (ObjectPage::Info, "Info", "s-info"),
        ]);
        let got = {
            let tab = tab.clone();
            move || info().with(|m| m.get(&tab).cloned()).unwrap_or_default()
        };
        let engine = state.connections.with_untracked(|l| {
            l.iter()
                .find(|c| c.config.id == connection)
                .map(|c| c.config.engine)
        });
        let dialect = engine.and_then(crate::editor::dialect_of);
        Some(view! {
            <div class="object-props">
                <nav class="props-nav" role="tablist" aria-orientation="vertical" aria-label="Properties">
                    {sections.into_iter().map(|(p, label, class)| {
                        let tab = tab.clone();
                        view! {
                            <button role="tab" class=class class:on=move || page.get() == p
                                aria-selected=move || (page.get() == p).to_string()
                                on:click=move |_| state.pages.update(|m| { m.insert(tab.clone(), p); })>{label}</button>
                        }
                    }).collect_view()}
                </nav>
                <div class="props-body">
                    {move || {
                        let got = got();
                        match page.get() {
                            ObjectPage::Ddl => ddl_view(got.ddl, dialect, &connection, &node).into_any(),
                            p => details_view(p, got.details, &node).into_any(),
                        }
                    }}
                </div>
            </div>
        })
    }
}

fn waiting() -> AnyView {
    view! { <p class="message">"Reading…"</p> }.into_any()
}

fn details_view(
    page: ObjectPage,
    details: Option<Result<ObjectDetails, String>>,
    node: &ObjectNode,
) -> AnyView {
    let d = match details {
        None => return waiting(),
        Some(Err(e)) => return view! { <p class="message error">{e}</p> }.into_any(),
        Some(Ok(d)) => d,
    };
    match page {
        ObjectPage::Columns => columns_view(&d),
        ObjectPage::Indexes => indexes_view(&d),
        ObjectPage::ForeignKeys => foreign_keys_view(&d),
        _ => info_view(&d, node),
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "" }
}

fn columns_view(d: &ObjectDetails) -> AnyView {
    if d.columns.is_empty() {
        return view! { <p class="message">"No columns to show."</p> }.into_any();
    }
    let in_key: HashMap<&str, &str> = d
        .foreign_keys
        .iter()
        .flat_map(|f| {
            f.columns
                .iter()
                .map(move |c| (c.as_str(), f.ref_table.as_str()))
        })
        .collect();
    let rows = d
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let key = match (c.primary_key, in_key.get(c.name.as_str())) {
                (true, Some(t)) => format!("PK, FK → {t}"),
                (true, None) => "PK".to_owned(),
                (false, Some(t)) => format!("FK → {t}"),
                (false, None) => String::new(),
            };
            view! {
                <tr>
                    <td class="num">{i + 1}</td>
                    <td class="name">{c.name.clone()}</td>
                    <td class="mono">{c.type_name.clone()}</td>
                    <td>{yes(!c.nullable)}</td>
                    <td class="mono">{c.default.clone().unwrap_or_default()}</td>
                    <td>{key}</td>
                    <td>{c.comment.clone().unwrap_or_default()}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <table class="props-table columns-table">
            <thead><tr><th>"#"</th><th>"Column"</th><th>"Type"</th><th>"Not null"</th><th>"Default"</th><th>"Key"</th><th>"Comment"</th></tr></thead>
            <tbody>{rows}</tbody>
        </table>
    }
    .into_any()
}

fn indexes_view(d: &ObjectDetails) -> AnyView {
    if d.indexes.is_empty() {
        return view! { <p class="message">"No indexes."</p> }.into_any();
    }
    let rows = d
        .indexes
        .iter()
        .map(|i| {
            let kind = if i.primary {
                "primary key"
            } else if i.unique {
                "unique"
            } else {
                ""
            };
            view! {
                <tr>
                    <td class="name">{i.name.clone()}</td>
                    <td class="mono">{i.columns.join(", ")}</td>
                    <td>{kind}</td>
                    <td>{i.method.clone().unwrap_or_default()}</td>
                    <td class="mono">{i.predicate.clone().unwrap_or_default()}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <table class="props-table indexes-table">
            <thead><tr><th>"Index"</th><th>"Columns"</th><th>"Kind"</th><th>"Method"</th><th>"Only where"</th></tr></thead>
            <tbody>{rows}</tbody>
        </table>
    }
    .into_any()
}

fn foreign_keys_view(d: &ObjectDetails) -> AnyView {
    if d.foreign_keys.is_empty() {
        return view! { <p class="message">"No foreign keys."</p> }.into_any();
    }
    let rows = d
        .foreign_keys
        .iter()
        .map(|f| {
            let table = match &f.ref_schema {
                Some(s) => format!("{s}.{}", f.ref_table),
                None => f.ref_table.clone(),
            };
            view! {
                <tr>
                    <td class="name">{f.name.clone()}</td>
                    <td class="mono">{f.columns.join(", ")}</td>
                    <td class="mono">{format!("{table} ({})", f.ref_columns.join(", "))}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <table class="props-table fk-table">
            <thead><tr><th>"Foreign key"</th><th>"Columns"</th><th>"References"</th></tr></thead>
            <tbody>{rows}</tbody>
        </table>
    }
    .into_any()
}

fn info_view(d: &ObjectDetails, node: &ObjectNode) -> AnyView {
    let mut facts: Vec<(String, String)> = vec![("Kind".into(), kind_label(node.kind).into())];
    let place = location(node);
    if !place.is_empty() {
        facts.push(("In".into(), place));
    }
    if let Some(n) = d.row_estimate {
        facts.push((
            "Rows".into(),
            format!("about {}", crate::grid::thousands(n)),
        ));
    }
    if let Some(n) = d.size_bytes {
        facts.push(("Size".into(), size_text(n)));
    }
    if let Some(c) = &d.comment {
        facts.push(("Comment".into(), c.clone()));
    }
    facts.extend(d.properties.iter().cloned());
    view! {
        <dl class="facts-list">
            {facts.into_iter().map(|(k, v)| view! { <dt>{k}</dt><dd>{v}</dd> }).collect_view()}
        </dl>
    }
    .into_any()
}

fn ddl_view(
    ddl: Option<Result<Option<String>, String>>,
    dialect: Option<treasury_core::driver::SqlDialect>,
    connection: &ConnectionId,
    node: &ObjectNode,
) -> AnyView {
    let text = match ddl {
        None => return waiting(),
        Some(Err(e)) => return view! { <p class="message error">{e}</p> }.into_any(),
        Some(Ok(None)) => {
            return view! { <p class="message">"The server gives no DDL for this object."</p> }
                .into_any();
        }
        Some(Ok(Some(t))) => t,
    };
    let state = ui();
    let copy = {
        let text = text.clone();
        move |_| {
            let text = text.clone();
            leptos::task::spawn_local(async move {
                let copied = ipc::to_clipboard(&text).await;
                state.notice.set(Some(if copied {
                    "Copied the DDL.".to_owned()
                } else {
                    "The clipboard refused the text.".to_owned()
                }));
            });
        }
    };
    let to_editor = {
        let (text, connection, title) = (text.clone(), connection.clone(), node.name.clone());
        move |_| {
            let (text, connection, title) = (text.clone(), connection.clone(), title.clone());
            leptos::task::spawn_local(async move {
                tabs::open(connection, format!("{title} DDL"), text, None).await;
            });
        }
    };
    view! {
        <div class="ddl">
            <div class="ddl-tools">
                <button class="btn small ddl-copy" on:click=copy>"Copy"</button>
                <button class="btn small ddl-open" on:click=to_editor>"Open in SQL editor"</button>
            </div>
            <pre class="ddl-text">{crate::editor::highlighted(dialect, &text)}</pre>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_read_naturally() {
        assert_eq!(super::size_text(512), "512 B");
        assert_eq!(super::size_text(65_536), "65.5 kB");
        assert_eq!(super::size_text(1_300_000), "1.3 MB");
    }
}
