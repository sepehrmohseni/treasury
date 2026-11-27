//! The database navigator: every saved connection as a node of one tree. Expanding a connection
//! opens it and lists its databases, schemas and objects; several can be open at once. A
//! double-click opens a table's editor, a right-click shows what can be done with a node.

use crate::ipc;
use crate::sidebar::{
    Link, add_connection, connect, env_class, env_label, import_connections, link_of, open,
};
use crate::state::{Dialog, Menu, MenuTarget, ObjectPage, ui};
use crate::tabs;
use leptos::prelude::*;
use std::collections::HashSet;
use treasury_core::config::{ConnectionConfig, ConnectionId};
use treasury_core::schema::{ChildPage, ChildQuery, ObjectKind, ObjectNode, ObjectPath};
use treasury_ipc::ConnectionView;
use wasm_bindgen::JsCast;

/// Objects whose editor shows their rows as well as their properties.
pub fn has_rows(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::MaterializedView
            | ObjectKind::Collection
    )
}

/// Objects a double-click opens an editor for.
pub fn has_editor(kind: ObjectKind) -> bool {
    has_rows(kind)
        || matches!(
            kind,
            ObjectKind::Function
                | ObjectKind::Procedure
                | ObjectKind::Sequence
                | ObjectKind::Type
                | ObjectKind::Trigger
                | ObjectKind::Event
                | ObjectKind::Index
                | ObjectKind::Constraint
                | ObjectKind::Extension
                | ObjectKind::Package
                | ObjectKind::Synonym
        )
}

/// The page an object's editor opens at: its columns when it has rows, otherwise its DDL.
pub fn first_page(kind: ObjectKind) -> ObjectPage {
    if has_rows(kind) {
        ObjectPage::Columns
    } else {
        ObjectPage::Ddl
    }
}

fn config_of(id: &ConnectionId) -> Option<ConnectionConfig> {
    ui().connections.with_untracked(|list| {
        list.iter()
            .find(|c| &c.config.id == id)
            .map(|c| c.config.clone())
    })
}

/// Expands or collapses connection `id`; expanding opens it.
fn toggle(id: &ConnectionId) {
    let state = ui();
    if state.expanded.with_untracked(|e| e.contains(id)) {
        state.expanded.update(|e| {
            e.remove(id);
        });
    } else {
        state.expanded.update(|e| {
            e.insert(id.clone());
        });
        if matches!(link_of(id), Link::Closed | Link::Failed) {
            connect(id.clone());
        }
    }
}

/// Closes connection `id`. Its tabs stay, to run on it again once it is opened.
fn disconnect(id: ConnectionId) {
    let state = ui();
    leptos::task::spawn_local(async move {
        let _ = ipc::call::<_, ()>("disconnect", &serde_json::json!({ "id": id })).await;
        state.live.update(|m| {
            m.remove(&id);
        });
        state.expanded.update(|e| {
            e.remove(&id);
        });
        crate::complete::fetch::forget(&id);
    });
}

/// Reads connection `id`'s objects again from the server.
fn refresh(id: &ConnectionId) {
    let state = ui();
    state.tree_rev.update(|m| {
        *m.entry(id.clone()).or_default() += 1;
    });
    crate::complete::fetch::forget(id);
}

/// Shows the value of a Redis key, or a statement for any other object, in a SQL editor tab.
fn show_rows(connection: &ConnectionId, node: &ObjectNode) {
    let state = ui();
    let Some(statement) =
        config_of(connection).and_then(|c| crate::editor::browse_statement(&c, node))
    else {
        return;
    };
    // A Redis key's tab is named by the whole key, not its last segment.
    let title = match node.kind {
        ObjectKind::Key => node
            .path
            .leaf()
            .map_or_else(|| node.name.clone(), |l| l.name.clone()),
        _ => node.name.clone(),
    };
    state.active.set(Some(connection.clone()));
    state.open_rows.set(Some((title, statement)));
}

/// Opens what a double-click on `node` opens: its editor, or a Redis key's value.
fn activate(connection: &ConnectionId, node: &ObjectNode) {
    if has_editor(node.kind) {
        tabs::open_object(connection.clone(), node.clone(), first_page(node.kind));
    } else if node.kind == ObjectKind::Key {
        show_rows(connection, node);
    }
}

fn show_menu(ev: &web_sys::MouseEvent, target: MenuTarget) {
    ev.prevent_default();
    ev.stop_propagation();
    ui().menu.set(Some(Menu {
        x: f64::from(ev.client_x()),
        y: f64::from(ev.client_y()),
        target,
    }));
}

#[component]
pub fn Navigator() -> impl IntoView {
    let state = ui();
    let shown = move || {
        let filter = state.nav_filter.get().to_lowercase();
        state.connections.with(|list| {
            list.iter()
                .filter(|c| filter.is_empty() || c.config.name.to_lowercase().contains(&filter))
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    let selected_connection = move || state.selected.get().map(|(c, _)| c);
    view! {
        <div class="side-actions nav-tools" role="toolbar" aria-label="Navigator">
            <button class="btn small primary" title="Add a connection" on:click=move |_| add_connection()>"New"</button>
            <button class="btn small" title="Import connections from a file" on:click=move |_| import_connections()>"Import"</button>
            <span class="push"></span>
            <button class="btn small nav-refresh" title="Read the selected connection's objects again"
                disabled=move || selected_connection().is_none_or(|c| link_of(&c) != Link::Open)
                on:click=move |_| if let Some(c) = selected_connection() { refresh(&c) }>"Refresh"</button>
            <button class="btn small nav-collapse" title="Collapse every connection"
                on:click=move |_| state.expanded.set(HashSet::new())>"Collapse"</button>
        </div>
        <div class="nav-filter">
            <input id="nav-filter" type="search" placeholder="Filter connections" aria-label="Filter connections"
                prop:value=move || state.nav_filter.get()
                on:input=move |ev| state.nav_filter.set(event_target_value(&ev)) />
        </div>
        <div class="list navigator" role="tree" aria-label="Database navigator">
            <For each=shown key=|c| c.config.id.clone() children=move |c| view! { <ConnectionNode view=c /> } />
            {move || state.connections.with(Vec::is_empty).then(|| view! {
                <p class="empty">"No connections yet. Add one, or import the ones another client exported."</p>
            })}
        </div>
    }
}

#[component]
fn ConnectionNode(view: ConnectionView) -> impl IntoView {
    let state = ui();
    let config = view.config;
    let id = config.id.clone();
    let link = Memo::new({
        let id = id.clone();
        move |_| link_of(&id)
    });
    let active = Memo::new({
        let id = id.clone();
        move |_| state.active.get().as_ref() == Some(&id)
    });
    let expanded = Memo::new({
        let id = id.clone();
        move |_| state.expanded.with(|e| e.contains(&id))
    });
    let selected = Memo::new({
        let id = id.clone();
        move |_| {
            state
                .selected
                .with(|s| s.as_ref().is_some_and(|(c, p)| *c == id && p.0.is_empty()))
        }
    });
    let rev = Memo::new({
        let id = id.clone();
        move |_| state.tree_rev.with(|m| m.get(&id).copied().unwrap_or(0))
    });
    let (click_id, twisty_id, menu_id, retry_id, tree_id) =
        (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    view! {
        <button
            class="item connection"
            role="treeitem"
            class:active=move || active.get()
            class:selected=move || selected.get()
            aria-current=move || active.get().then_some("true")
            aria-expanded=move || expanded.get().to_string()
            title="Open; right-click for more"
            on:click=move |_| {
                state.selected.set(Some((click_id.clone(), ObjectPath::root())));
                open(click_id.clone());
            }
            on:contextmenu=move |ev| show_menu(&ev, MenuTarget::Connection(menu_id.clone()))
        >
            <span class="twisty" on:click=move |ev| {
                ev.stop_propagation();
                state.selected.set(Some((twisty_id.clone(), ObjectPath::root())));
                toggle(&twisty_id);
            }>{move || if expanded.get() { "▾" } else { "▸" }}</span>
            {move || {
                let (class, label) = link.get().marker();
                view! { <span class=class title=label><span class="visually-hidden">{label}</span></span> }
            }}
            <span class="name">{config.name.clone()}</span>
            <span class=env_class(config.environment)>{env_label(config.environment)}</span>
        </button>
        <Show when=move || expanded.get()>
            {
                let id = tree_id.clone();
                let retry = retry_id.clone();
                move || {
                    let retry = retry.clone();
                    match link.get() {
                        Link::Open => {
                            let id = id.clone();
                            view! {
                                <div class="tree" role="group">
                                    {move || {
                                        rev.track();
                                        view! { <TreeLevel connection=id.clone() path=ObjectPath::root() depth=1 /> }
                                    }}
                                </div>
                            }.into_any()
                        }
                        Link::Opening => view! {
                            <div class="item detail conn-state">"Connecting…"</div>
                        }.into_any(),
                        Link::Failed => view! {
                            <div class="item conn-state failed">
                                <span>"Could not connect"</span>
                                <button class="btn small retry" on:click=move |_| connect(retry.clone())>"Try again"</button>
                            </div>
                        }.into_any(),
                        Link::Closed => view! {
                            <div class="item conn-state">
                                <span class="detail">"Not connected"</span>
                                <button class="btn small" on:click=move |_| connect(retry.clone())>"Connect"</button>
                            </div>
                        }.into_any(),
                    }
                }
            }
        </Show>
    }
}

fn indent(depth: usize) -> String {
    format!("{}px", 12 + depth * 14)
}

/// The children of `path`, loaded when shown, with a "More" row for further pages.
#[component]
fn TreeLevel(connection: ConnectionId, path: ObjectPath, depth: usize) -> impl IntoView {
    let nodes = RwSignal::new(Vec::<ObjectNode>::new());
    let next = RwSignal::new(None::<String>);
    let failed = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let load = {
        let connection = connection.clone();
        let path = path.clone();
        move |cursor: Option<String>| {
            let connection = connection.clone();
            let path = path.clone();
            loading.set(true);
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
                    // The level may have been collapsed while it was read.
                    Ok(page) => {
                        nodes.try_update(|n| n.extend(page.nodes));
                        next.try_set(page.next_cursor);
                    }
                    Err(e) => {
                        failed.try_set(Some(e.to_string()));
                    }
                }
                loading.try_set(false);
            });
        }
    };
    load(None);
    let more = load.clone();
    view! {
        <For
            each=move || nodes.get()
            key=|n| n.path.clone()
            children={
                let connection = connection.clone();
                move |n| view! { <TreeNode connection=connection.clone() node=n depth=depth /> }
            }
        />
        {move || (loading.get() && nodes.with(Vec::is_empty)).then(|| view! {
            <div class="item detail" style:padding-inline-start=indent(depth + 1)>"Loading…"</div>
        })}
        {move || (!loading.get() && nodes.with(Vec::is_empty) && failed.with(Option::is_none)).then(|| view! {
            <div class="item detail empty-node" style:padding-inline-start=indent(depth + 1)>"Empty"</div>
        })}
        {move || next.get().map(|c| {
            let more = more.clone();
            view! {
                <button class="item" style:padding-inline-start=indent(depth + 1)
                    on:click=move |_| more(Some(c.clone()))>"More…"</button>
            }
        })}
        {move || failed.get().map(|e| view! {
            <div class="item detail failed" style:padding-inline-start=indent(depth + 1) title=e.clone()>{format!("Could not load: {e}")}</div>
        })}
    }
}

#[component]
fn TreeNode(connection: ConnectionId, node: ObjectNode, depth: usize) -> impl IntoView {
    let state = ui();
    let open = RwSignal::new(false);
    let expandable = node.expandable;
    let opens = has_editor(node.kind) || node.kind == ObjectKind::Key;
    let path = node.path.clone();
    let selected = Memo::new({
        let (connection, path) = (connection.clone(), path.clone());
        move |_| {
            state.selected.with(|s| {
                s.as_ref()
                    .is_some_and(|(c, p)| *c == connection && *p == path)
            })
        }
    });
    let select = {
        let (connection, path) = (connection.clone(), path.clone());
        move || state.selected.set(Some((connection.clone(), path.clone())))
    };
    let click = {
        let select = select.clone();
        // Folders open with a click; objects with an editor open their children with the
        // arrow, since a double-click opens the editor.
        move |_| {
            select();
            if expandable && !opens {
                open.update(|o| *o = !*o);
            }
        }
    };
    let twisty = {
        let select = select.clone();
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            select();
            if expandable {
                open.update(|o| *o = !*o);
            }
        }
    };
    let dbl = {
        let (connection, node) = (connection.clone(), node.clone());
        move |_| activate(&connection, &node)
    };
    let menu = {
        let (connection, node, select) = (connection.clone(), node.clone(), select.clone());
        move |ev: web_sys::MouseEvent| {
            select();
            show_menu(&ev, MenuTarget::Object(connection.clone(), node.clone()));
        }
    };
    view! {
        <button
            class="item"
            class:selected=move || selected.get()
            role="treeitem"
            aria-expanded=move || expandable.then(|| open.get().to_string())
            style:padding-inline-start=indent(depth)
            title=if opens { "Double-click to open; right-click for more" } else { "" }
            on:click=click
            on:dblclick=dbl
            on:contextmenu=menu
        >
            <span class="twisty" on:click=twisty>{move || if !expandable { " " } else if open.get() { "▾" } else { "▸" }}</span>
            <span class="name">{node.name.clone()}</span>
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

/// One entry of a right-click menu.
struct Entry {
    label: &'static str,
    class: &'static str,
    run: Box<dyn Fn()>,
}

fn entry(label: &'static str, class: &'static str, run: impl Fn() + 'static) -> Entry {
    Entry {
        label,
        class,
        run: Box::new(run),
    }
}

fn entries(target: &MenuTarget) -> Vec<Entry> {
    let state = ui();
    match target.clone() {
        MenuTarget::Connection(id) => {
            let open_now = link_of(&id) == Link::Open;
            let mut list = Vec::new();
            {
                let id = id.clone();
                list.push(if open_now {
                    entry("Disconnect", "m-disconnect", move || disconnect(id.clone()))
                } else {
                    entry("Connect", "m-connect", move || {
                        state.expanded.update(|e| {
                            e.insert(id.clone());
                        });
                        open(id.clone());
                    })
                });
            }
            {
                let id = id.clone();
                list.push(entry("Open SQL editor", "m-sql", move || {
                    open(id.clone());
                    tabs::new_tab();
                }));
            }
            if open_now {
                let id = id.clone();
                list.push(entry("Refresh", "m-refresh", move || refresh(&id)));
            }
            list.push(entry("Edit connection…", "m-edit", move || {
                if let Some(c) = config_of(&id) {
                    state.dialog.set(Dialog::Connection(Box::new(c)));
                }
            }));
            list
        }
        MenuTarget::Object(connection, node) => {
            let mut list = Vec::new();
            if has_editor(node.kind) {
                let (c, n) = (connection.clone(), node.clone());
                list.push(entry("Open", "m-open", move || {
                    tabs::open_object(c.clone(), n.clone(), first_page(n.kind));
                }));
            }
            if has_rows(node.kind) {
                let (c, n) = (connection.clone(), node.clone());
                list.push(entry("View data", "m-data", move || {
                    tabs::open_object(c.clone(), n.clone(), ObjectPage::Data);
                }));
                if let Some(statement) = config_of(&connection)
                    .and_then(|cfg| crate::editor::browse_statement(&cfg, &node))
                {
                    let (c, title) = (connection.clone(), node.name.clone());
                    list.push(entry("Read in a SQL editor", "m-select", move || {
                        let (c, title, statement) = (c.clone(), title.clone(), statement.clone());
                        state.active.set(Some(c.clone()));
                        leptos::task::spawn_local(async move {
                            tabs::open(c, title, statement, None).await;
                        });
                    }));
                }
            }
            if node.kind == ObjectKind::Key {
                let (c, n) = (connection.clone(), node.clone());
                list.push(entry("Open", "m-open", move || show_rows(&c, &n)));
            }
            {
                let name = node.name.clone();
                list.push(entry("Copy name", "m-copy", move || {
                    let name = name.clone();
                    leptos::task::spawn_local(async move {
                        let copied = ipc::to_clipboard(&name).await;
                        state.notice.set(Some(if copied {
                            format!("Copied \"{name}\".")
                        } else {
                            "The clipboard refused the text.".to_owned()
                        }));
                    });
                }));
            }
            list.push(entry("Refresh", "m-refresh", move || refresh(&connection)));
            list
        }
    }
}

/// The right-click menu, closed by a click elsewhere or Escape.
#[component]
pub fn ContextMenu() -> impl IntoView {
    let state = ui();
    let _ = window_event_listener(leptos::ev::mousedown, move |ev| {
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|e| e.closest(".context-menu").ok().flatten())
            .is_some();
        if !inside && state.menu.with_untracked(Option::is_some) {
            state.menu.set(None);
        }
    });
    let _ = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && state.menu.with_untracked(Option::is_some) {
            state.menu.set(None);
        }
    });
    move || {
        state.menu.get().map(|m| {
            let items = entries(&m.target);
            // Kept inside the window near its right and bottom edges.
            let (w, h) = (220.0, 30.0 * items.len() as f64 + 8.0);
            let window = web_sys::window();
            let width = window
                .as_ref()
                .and_then(|w| w.inner_width().ok())
                .and_then(|v| v.as_f64())
                .unwrap_or(1280.0);
            let height = window
                .as_ref()
                .and_then(|w| w.inner_height().ok())
                .and_then(|v| v.as_f64())
                .unwrap_or(800.0);
            let x = m.x.min(width - w - 4.0).max(0.0);
            let y = m.y.min(height - h - 4.0).max(0.0);
            view! {
                <ul class="context-menu" role="menu" style:left=format!("{x}px") style:top=format!("{y}px")>
                    {items.into_iter().map(|e| {
                        let run = e.run;
                        view! {
                            <li role="none">
                                <button role="menuitem" class=e.class on:click=move |_| {
                                    state.menu.set(None);
                                    run();
                                }>{e.label}</button>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
        })
    }
}
