//! The sidebar: the database navigator, saved queries and history, and opening connections.

use crate::ipc;
use crate::state::{Dialog, SideView, ui};
use crate::tabs;
use leptos::prelude::*;
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine, Environment};
use treasury_ipc::{ImportPreview, LiveView, SavedQueryView};

pub fn env_class(e: Environment) -> &'static str {
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

/// Makes connection `id` the active one and expands it in the navigator, and opens it unless
/// it is open or opening.
pub fn open(id: ConnectionId) {
    let state = ui();
    state.active.set(Some(id.clone()));
    if !state.expanded.with_untracked(|e| e.contains(&id)) {
        state.expanded.update(|e| {
            e.insert(id.clone());
        });
    }
    if state.live.with(|m| m.contains_key(&id)) || state.connecting.with(|c| c.contains(&id)) {
        return;
    }
    connect(id);
}

/// Opens connection `id`. Why it failed, when it does, is kept for the sidebar and the
/// workspace to show until the next try.
pub fn connect(id: ConnectionId) {
    let state = ui();
    state.failed.update(|m| {
        m.remove(&id);
    });
    state.connecting.update(|c| {
        c.insert(id.clone());
    });
    leptos::task::spawn_local(async move {
        let result = ipc::call::<_, LiveView>("connect", &serde_json::json!({ "id": id })).await;
        state.connecting.update(|c| {
            c.remove(&id);
        });
        match result {
            Ok(l) => {
                state.live.update(|m| {
                    m.insert(id, l);
                });
            }
            Err(e) => state.failed.update(|m| {
                m.insert(id, e.to_string());
            }),
        }
    });
}

/// How a connection stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Open,
    Opening,
    Failed,
    Closed,
}

pub fn link_of(id: &ConnectionId) -> Link {
    let state = ui();
    if state.live.with(|m| m.contains_key(id)) {
        Link::Open
    } else if state.connecting.with(|c| c.contains(id)) {
        Link::Opening
    } else if state.failed.with(|m| m.contains_key(id)) {
        Link::Failed
    } else {
        Link::Closed
    }
}

impl Link {
    /// The marker's class and what it says.
    pub fn marker(self) -> (&'static str, &'static str) {
        match self {
            Link::Open => ("link link-open", "Connected"),
            Link::Opening => ("link link-opening", "Connecting"),
            Link::Failed => ("link link-failed", "Could not connect"),
            Link::Closed => ("link", "Not connected"),
        }
    }
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
            <div class="side-views" role="tablist" aria-label="Sidebar">
                <button role="tab" class="view-connections" class:on=move || state.side_view.get() == SideView::Connections
                    aria-selected=move || (state.side_view.get() == SideView::Connections).to_string()
                    on:click=move |_| state.side_view.set(SideView::Connections)>"Navigator"</button>
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
            <div class="nav-pane" hidden=move || state.side_view.get() != SideView::Connections>
                <crate::navigator::Navigator />
            </div>
            <SideResize />
        </aside>
    }
}

/// The sidebar's right edge, dragged to make it wider or narrower.
#[component]
fn SideResize() -> impl IntoView {
    let state = ui();
    let dragging = RwSignal::new(false);
    let _ = window_event_listener(leptos::ev::mousemove, move |ev| {
        if dragging.get_untracked() {
            let w = u32::try_from(ev.client_x().clamp(200, 640))
                .unwrap_or(crate::state::DEFAULT_SIDE_WIDTH);
            state.side_width.set(w);
        }
    });
    let _ = window_event_listener(leptos::ev::mouseup, move |_| {
        if dragging.get_untracked() {
            dragging.set(false);
            crate::home::set_setting(
                "side_width",
                serde_json::Value::from(state.side_width.get_untracked()),
            );
        }
    });
    view! {
        <div class="side-resize" role="separator" aria-orientation="vertical" aria-label="Sidebar width"
            title="Drag to resize; double-click to reset"
            class:dragging=move || dragging.get()
            on:mousedown=move |ev| { ev.prevent_default(); dragging.set(true); }
            on:dblclick=move |_| {
                state.side_width.set(crate::state::DEFAULT_SIDE_WIDTH);
                crate::home::set_setting("side_width", serde_json::Value::from(crate::state::DEFAULT_SIDE_WIDTH));
            }></div>
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
