//! Saved connections and, under the open one, its objects.

use crate::ipc;
use crate::state::{Dialog, SideView, ui};
use crate::tabs;
use leptos::prelude::*;
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine, Environment};
use treasury_core::schema::{ChildPage, ChildQuery, ObjectNode, ObjectPath};
use treasury_ipc::{ImportPreview, LiveView, SavedQueryView};

fn env_class(e: Environment) -> &'static str {
    match e {
        Environment::Local => "env env-local",
        Environment::Development => "env env-development",
        Environment::Staging => "env env-staging",
        Environment::Production => "env env-production",
    }
}

pub fn env_label(e: Environment) -> &'static str {
    match e {
        Environment::Local => "local",
        Environment::Development => "dev",
        Environment::Staging => "staging",
        Environment::Production => "production",
    }
}

/// Opens the form for a new connection.
pub fn add_connection() {
    let state = ui();
    leptos::task::spawn_local(async move {
        match ipc::call::<_, ConnectionConfig>(
            "new_connection",
            &serde_json::json!({ "engine": Engine::Postgres }),
        )
        .await
        {
            Ok(c) => state.dialog.set(Dialog::Connection(Box::new(c))),
            Err(e) => state.notice.set(Some(e.to_string())),
        }
    });
}

/// Asks for an export file and shows what it holds.
pub fn import_connections() {
    let state = ui();
    leptos::task::spawn_local(async move {
        let Some(path) = ipc::pick_file("Import connections").await else {
            return;
        };
        match ipc::call::<_, ImportPreview>("import_preview", &serde_json::json!({ "path": path }))
            .await
        {
            Ok(preview) => state.dialog.set(Dialog::Import { path, preview }),
            Err(e) => state.notice.set(Some(e.to_string())),
        }
    });
}

/// Opens connection `id` and makes it the active one.
pub fn open(id: ConnectionId) {
    let state = ui();
    state.active.set(Some(id.clone()));
    if state.live.with(|m| m.contains_key(&id)) {
        return;
    }
    state.connecting.set(Some(id.clone()));
    leptos::task::spawn_local(async move {
        let result = ipc::call::<_, LiveView>("connect", &serde_json::json!({ "id": id })).await;
        state.connecting.set(None);
        match result {
            Ok(l) => {
                state.live.update(|m| {
                    m.insert(id, l);
                });
                state.notice.set(None);
            }
            Err(e) => state.notice.set(Some(e.to_string())),
        }
    });
}

#[component]
pub fn Sidebar() -> impl IntoView {
    let state = ui();
    view! {
        <aside class="sidebar" aria-label="Connections">
            <div class="brand">
                <img src="icon.svg" alt="" />"Treasury"
                <button class="btn small push settings" on:click=move |_| state.dialog.set(Dialog::Settings)>"Settings"</button>
            </div>
            <div class="side-actions">
                <button class="btn small primary" on:click=move |_| add_connection()>"Add"</button>
                <button class="btn small" on:click=move |_| import_connections()>"Import"</button>
            </div>
            <div class="side-views" role="tablist" aria-label="Sidebar">
                <button role="tab" class="view-connections" class:on=move || state.side_view.get() == SideView::Connections
                    aria-selected=move || (state.side_view.get() == SideView::Connections).to_string()
                    on:click=move |_| state.side_view.set(SideView::Connections)>"Connections"</button>
                <button role="tab" class="view-queries" class:on=move || state.side_view.get() == SideView::Queries
                    aria-selected=move || (state.side_view.get() == SideView::Queries).to_string()
                    on:click=move |_| state.side_view.set(SideView::Queries)>"Queries"</button>
                <button role="tab" class="view-history" class:on=move || state.side_view.get() == SideView::History
                    aria-selected=move || (state.side_view.get() == SideView::History).to_string()
                    on:click=move |_| state.side_view.set(SideView::History)>"History"</button>
            </div>
            <Show when=move || state.side_view.get() == SideView::Queries>
                <SavedQueries />
            </Show>
            <Show when=move || state.side_view.get() == SideView::History>
                <crate::history::HistoryPanel />
            </Show>
            <div class="list" hidden=move || state.side_view.get() != SideView::Connections>
                <div class="side-section">"Connections"</div>
                <For
                    each=move || state.connections.get()
                    key=|c| c.config.id.clone()
                    children=move |c| {
                        let id = c.config.id.clone();
                        let id_active = id.clone();
                        let id_tree = id.clone();
                        let edit = c.config.clone();
                        view! {
                            <button
                                class="item"
                                class:active=move || state.active.get().as_ref() == Some(&id_active)
                                on:click=move |_| open(id.clone())
                                on:dblclick=move |_| state.dialog.set(Dialog::Connection(Box::new(edit.clone())))
                                title="Open; double-click to edit"
                            >
                                <span>{c.config.name.clone()}</span>
                                <span class=env_class(c.config.environment)>{env_label(c.config.environment)}</span>
                            </button>
                            <Show when=move || {
                                state.active.get().as_ref() == Some(&id_tree)
                                    && state.live.with(|m| m.contains_key(&id_tree))
                            }>
                                <div class="tree" role="tree">
                                    <TreeLevel connection=c.config.id.clone() path=ObjectPath::root() depth=0 />
                                </div>
                            </Show>
                        }
                    }
                />
            </div>
        </aside>
    }
}

/// The children of `path`, loaded when shown, with a "More" row for further pages.
#[component]
fn TreeLevel(connection: ConnectionId, path: ObjectPath, depth: usize) -> impl IntoView {
    let state = ui();
    let nodes = RwSignal::new(Vec::<ObjectNode>::new());
    let next = RwSignal::new(None::<String>);
    let failed = RwSignal::new(None::<String>);
    let load = {
        let connection = connection.clone();
        let path = path.clone();
        move |cursor: Option<String>| {
            let connection = connection.clone();
            let path = path.clone();
            leptos::task::spawn_local(async move {
                let query = ChildQuery {
                    filter: None,
                    cursor,
                    limit: 200,
                };
                match ipc::call::<_, ChildPage>(
                    "children",
                    &serde_json::json!({ "id": connection, "path": path, "query": query }),
                )
                .await
                {
                    Ok(page) => {
                        nodes.update(|n| n.extend(page.nodes));
                        next.set(page.next_cursor);
                    }
                    Err(e) => failed.set(Some(e.to_string())),
                }
            });
        }
    };
    load(None);
    let more = load.clone();
    let _ = state;
    view! {
        <For
            each=move || nodes.get()
            key=|n| n.path.clone()
            children={
                let connection = connection.clone();
                move |n| view! { <TreeNode connection=connection.clone() node=n depth=depth /> }
            }
        />
        {move || next.get().map(|c| {
            let more = more.clone();
            view! {
                <button class="item" style:padding-inline-start=format!("{}px", 12 + (depth + 1) * 14)
                    on:click=move |_| more(Some(c.clone()))>"More…"</button>
            }
        })}
        {move || failed.get().map(|e| view! { <div class="item detail" title=e.clone()>"Could not load"</div> })}
    }
}

#[component]
fn TreeNode(connection: ConnectionId, node: ObjectNode, depth: usize) -> impl IntoView {
    let state = ui();
    let open = RwSignal::new(false);
    let expandable = node.expandable;
    let path = node.path.clone();
    let browse = node.clone();
    let show_rows = move |_| {
        if let Some(statement) = state
            .active_config()
            .and_then(|c| crate::editor::browse_statement(&c, &browse))
        {
            // A Redis key's tab is named by the whole key, not its last segment.
            let title = match browse.kind {
                treasury_core::schema::ObjectKind::Key => browse
                    .path
                    .leaf()
                    .map_or_else(|| browse.name.clone(), |l| l.name.clone()),
                _ => browse.name.clone(),
            };
            state.open_rows.set(Some((title, statement)));
        }
    };
    view! {
        <button
            class="item"
            role="treeitem"
            aria-expanded=move || expandable.then(|| open.get().to_string())
            style:padding-inline-start=format!("{}px", 12 + depth * 14)
            on:click=move |_| if expandable { open.update(|o| *o = !*o) }
            on:dblclick=show_rows
        >
            <span class="twisty">{move || if !expandable { " " } else if open.get() { "▾" } else { "▸" }}</span>
            <span>{node.name.clone()}</span>
            {node.detail.clone().map(|d| view! { <span class="detail">{d}</span> })}
        </button>
        <Show when=move || open.get()>
            {
                let connection = connection.clone();
                let path = path.clone();
                move || view! { <TreeLevel connection=connection.clone() path=path.clone() depth=depth + 1 /> }.into_any()
            }
        </Show>
    }
}

/// Saved queries of the active connection and those shown on every connection, by folder.
#[component]
fn SavedQueries() -> impl IntoView {
    let state = ui();
    let groups = move || {
        let active = state.active.get();
        let mut groups: Vec<(String, Vec<SavedQueryView>)> = Vec::new();
        state.queries.with(|list| {
            for q in list
                .iter()
                .filter(|q| q.connection.is_none() || q.connection == active)
            {
                match groups.last_mut() {
                    Some((folder, items)) if *folder == q.folder => items.push(q.clone()),
                    _ => groups.push((q.folder.clone(), vec![q.clone()])),
                }
            }
        });
        groups
    };
    view! {
        <div class="list queries">
            {move || {
                if state.active.get().is_none() {
                    return view! { <p class="empty">"Open a connection to see its saved queries."</p> }.into_any();
                }
                let groups = groups();
                if groups.is_empty() {
                    return view! {
                        <p class="empty">"No saved queries. Ctrl+S in the editor saves the tab as one."</p>
                    }.into_any();
                }
                groups.into_iter().map(|(folder, items)| view! {
                    {(!folder.is_empty()).then(|| view! { <div class="side-section folder">{folder.clone()}</div> })}
                    {items.into_iter().map(|q| {
                        let (open, edit) = (q.clone(), q.clone());
                        let shared = q.connection.is_none();
                        view! {
                            <button class="item query" title="Open; double-click to rename, move or delete"
                                on:click=move |_| tabs::open_saved(open.clone())
                                on:dblclick=move |_| state.dialog.set(Dialog::SaveQuery { tab: None, query: Some(edit.clone()) })>
                                <span>{q.name.clone()}</span>
                                {shared.then(|| view! { <span class="detail">"every connection"</span> })}
                            </button>
                        }
                    }).collect_view()}
                }).collect_view().into_any()
            }}
        </div>
    }
}
