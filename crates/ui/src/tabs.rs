//! Editor tabs: several per connection, each with its own result. The app keeps every tab's text
//! as it is typed, so tabs come back after a restart or a crash. A tab can show a saved query;
//! saving it again updates that query.

use crate::ipc;
use crate::state::{Dialog, Output, Tab, ui};
use leptos::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use treasury_core::config::ConnectionId;
use treasury_ipc::{SavedQueryView, TabView, new_tab_title};

/// How long typing pauses before the tab is kept.
const KEEP_AFTER: Duration = Duration::from_millis(300);

thread_local! {
    /// Connections whose first tab is being opened, so it is opened once.
    static OPENING: RefCell<HashSet<ConnectionId>> = RefCell::new(HashSet::new());
    /// A count of each tab's edits, so only the last edit before a pause keeps the tab.
    static EDITS: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
}

fn tab(id: &str) -> Option<Tab> {
    ui().tabs
        .with_untracked(|t| t.iter().find(|t| t.id == id).cloned())
}

/// The text of tab `id`.
pub fn text_of(id: &str) -> String {
    ui().tab_text
        .with_untracked(|m| m.get(id).cloned())
        .unwrap_or_default()
}

fn add(view: TabView, position: u32) {
    let state = ui();
    let id = view.id.clone();
    state.tab_text.update(|m| {
        m.insert(id.clone(), view.text);
    });
    state.cursors.update(|m| {
        m.insert(id.clone(), view.cursor);
    });
    state.outputs.update(|m| {
        m.insert(id.clone(), Output::new());
    });
    state.tabs.update(|t| {
        t.push(Tab {
            id,
            connection: view.connection,
            title: view.title,
            saved_query: view.saved_query,
            position,
        });
    });
}

/// Reads the tabs the app kept and the saved queries, at start.
pub fn load() {
    let state = ui();
    leptos::task::spawn_local(async move {
        match ipc::call0::<Vec<(TabView, u32)>>("tabs").await {
            Ok(list) => {
                for (view, position) in list {
                    let (connection, id) = (view.connection.clone(), view.id.clone());
                    if let Some(q) = view.saved_query.clone() {
                        load_saved_text(q);
                    }
                    add(view, position);
                    state.current_tab.update(|m| {
                        m.entry(connection).or_insert(id);
                    });
                }
            }
            Err(e) => state.notice.set(Some(e.to_string())),
        }
        state.tabs_loaded.set(true);
    });
    state.reload_queries();
}

/// Reads the saved text of query `id`, to tell when a tab showing it has changed. A query that
/// is gone no longer belongs to its tabs, which keep their text.
fn load_saved_text(id: String) {
    let state = ui();
    leptos::task::spawn_local(async move {
        match ipc::call::<_, String>("saved_query_text", &serde_json::json!({ "id": id })).await {
            Ok(text) => state.saved_text.update(|m| {
                m.insert(id, text);
            }),
            Err(_) => unlink(&id),
        }
    });
}

/// Opens a tab on `connection` after every other tab, shows it, and returns its identifier.
pub async fn open(
    connection: ConnectionId,
    title: String,
    text: String,
    saved_query: Option<String>,
) -> Option<String> {
    let state = ui();
    let position = state
        .tabs
        .with_untracked(|t| t.iter().map(|t| t.position + 1).max().unwrap_or(0));
    let args = serde_json::json!({
        "connection": connection, "title": title, "text": text,
        "savedQuery": saved_query, "position": position,
    });
    match ipc::call::<_, TabView>("open_tab", &args).await {
        Ok(view) => {
            let id = view.id.clone();
            add(view, position);
            state.current_tab.update(|m| {
                m.insert(connection, id.clone());
            });
            Some(id)
        }
        Err(e) => {
            state.notice.set(Some(e.to_string()));
            None
        }
    }
}

/// A new, empty tab on the active connection.
pub fn new_tab() {
    let state = ui();
    let Some(connection) = state.active.get_untracked() else {
        return;
    };
    let title = state.tabs.with_untracked(|t| {
        new_tab_title(
            t.iter()
                .filter(|t| t.connection == connection)
                .map(|t| t.title.as_str()),
        )
    });
    leptos::task::spawn_local(async move {
        open(connection, title, String::new(), None).await;
    });
}

/// Gives the active connection a tab to show: the first of its tabs, or a new one.
pub fn ensure_tab() {
    let state = ui();
    let (Some(connection), true) = (state.active.get(), state.tabs_loaded.get()) else {
        return;
    };
    let first = state.tabs.with(|t| {
        t.iter()
            .find(|t| t.connection == connection)
            .map(|t| t.id.clone())
    });
    let current = state.current_tab.with(|m| m.get(&connection).cloned());
    match first {
        Some(first) => {
            if current.is_none_or(|c| tab(&c).is_none()) {
                state.current_tab.update(|m| {
                    m.insert(connection, first);
                });
            }
        }
        None => {
            if !OPENING.with(|o| o.borrow_mut().insert(connection.clone())) {
                return;
            }
            leptos::task::spawn_local(async move {
                open(connection.clone(), new_tab_title([]), String::new(), None).await;
                OPENING.with(|o| o.borrow_mut().remove(&connection));
            });
        }
    }
}

/// Shows tab `id`, keeping the one it replaces.
pub fn show(id: String) {
    let state = ui();
    let Some(t) = tab(&id) else {
        return;
    };
    if let Some(old) = state
        .current_tab
        .with_untracked(|m| m.get(&t.connection).cloned())
        .filter(|old| *old != id)
    {
        keep(&old);
    }
    state.current_tab.update(|m| {
        m.insert(t.connection, id);
    });
}

/// Records new text for tab `id`, kept once typing pauses.
pub fn set_text(id: &str, text: String) {
    ui().tab_text.update(|m| {
        m.insert(id.to_owned(), text);
    });
    let id = id.to_owned();
    let edit = EDITS.with(|e| {
        let mut e = e.borrow_mut();
        let n = e.entry(id.clone()).or_default();
        *n += 1;
        *n
    });
    set_timeout(
        move || {
            if EDITS.with(|e| e.borrow().get(&id) == Some(&edit)) {
                keep(&id);
            }
        },
        KEEP_AFTER,
    );
}

/// Records where the cursor of tab `id` is.
pub fn set_cursor(id: &str, at: u32) {
    ui().cursors.update(|m| {
        m.insert(id.to_owned(), at);
    });
}

/// Has the app keep tab `id` as it is now.
pub fn keep(id: &str) {
    let state = ui();
    let Some(t) = tab(id) else {
        return;
    };
    let view = TabView {
        id: t.id,
        connection: t.connection,
        title: t.title,
        text: text_of(id),
        cursor: state
            .cursors
            .with_untracked(|m| m.get(id).copied())
            .unwrap_or(0),
        saved_query: t.saved_query,
    };
    leptos::task::spawn_local(async move {
        let args = serde_json::json!({ "tab": view, "position": t.position });
        if let Err(e) = ipc::call::<_, ()>("save_tab", &args).await {
            state.notice.set(Some(e.to_string()));
        }
    });
}

/// Whether tab `id` holds text that is not saved as a query.
pub fn text_unsaved(id: &str) -> bool {
    let state = ui();
    let text = text_of(id);
    if text.trim().is_empty() {
        return false;
    }
    match tab(id).and_then(|t| t.saved_query) {
        Some(q) => state
            .saved_text
            .with_untracked(|m| m.get(&q) != Some(&text)),
        None => true,
    }
}

/// Closes tab `id`, after asking when that would lose work kept nowhere else.
pub fn request_close(id: String) {
    let state = ui();
    let unsaved_text = text_unsaved(&id);
    let pending = state
        .outputs
        .with_untracked(|m| m.get(&id).map(|o| o.pending.get_untracked()))
        .unwrap_or(0);
    if unsaved_text || pending > 0 {
        state.dialog.set(Dialog::CloseTab {
            tab: id,
            unsaved_text,
            pending,
        });
    } else {
        close(id);
    }
}

/// Closes tab `id` and shows its neighbour.
pub fn close(id: String) {
    let state = ui();
    let Some(t) = tab(&id) else {
        return;
    };
    let siblings: Vec<String> = state.tabs.with_untracked(|all| {
        all.iter()
            .filter(|x| x.connection == t.connection)
            .map(|x| x.id.clone())
            .collect()
    });
    let at = siblings.iter().position(|x| *x == id).unwrap_or(0);
    let next = siblings
        .get(at + 1)
        .or_else(|| at.checked_sub(1).and_then(|i| siblings.get(i)))
        .cloned();
    state.tabs.update(|all| all.retain(|x| x.id != id));
    if let Some(out) = state.outputs.try_update(|m| m.remove(&id)).flatten()
        && let Some(rid) = out.result.get_untracked()
    {
        leptos::task::spawn_local(async move {
            let _ = ipc::call::<_, ()>("close_result", &serde_json::json!({ "result": rid })).await;
        });
    }
    state.tab_text.update(|m| {
        m.remove(&id);
    });
    state.cursors.update(|m| {
        m.remove(&id);
    });
    if state
        .current_tab
        .with_untracked(|m| m.get(&t.connection) == Some(&id))
    {
        state.current_tab.update(|m| match next {
            Some(n) => {
                m.insert(t.connection, n);
            }
            None => {
                m.remove(&t.connection);
            }
        });
    }
    leptos::task::spawn_local(async move {
        if let Err(e) = ipc::call::<_, ()>("close_tab", &serde_json::json!({ "id": id })).await {
            state.notice.set(Some(e.to_string()));
        }
    });
}

/// Opens saved query `q` on the active connection, or shows the tab that already has it.
pub fn open_saved(q: SavedQueryView) {
    let state = ui();
    let Some(connection) = state.active.get_untracked() else {
        return;
    };
    let open_tab = state.tabs.with_untracked(|t| {
        t.iter()
            .find(|t| t.connection == connection && t.saved_query.as_ref() == Some(&q.id))
            .map(|t| t.id.clone())
    });
    if let Some(id) = open_tab {
        show(id);
        return;
    }
    leptos::task::spawn_local(async move {
        match ipc::call::<_, String>("saved_query_text", &serde_json::json!({ "id": q.id })).await {
            Ok(text) => {
                state.saved_text.update(|m| {
                    m.insert(q.id.clone(), text.clone());
                });
                open(connection, q.name, text, Some(q.id)).await;
            }
            Err(e) => state.notice.set(Some(e.to_string())),
        }
    });
}

/// Saves the shown tab: again over the query it shows, or as a new query after asking for a
/// name.
pub fn save_current() {
    let state = ui();
    let Some(id) = state.current_untracked() else {
        return;
    };
    let query = tab(&id).and_then(|t| t.saved_query).and_then(|q| {
        state
            .queries
            .with_untracked(|l| l.iter().find(|x| x.id == q).cloned())
    });
    let Some(q) = query else {
        state.dialog.set(Dialog::SaveQuery {
            tab: Some(id),
            query: None,
        });
        return;
    };
    let text = text_of(&id);
    leptos::task::spawn_local(async move {
        let args = serde_json::json!({
            "id": q.id, "name": q.name, "folder": q.folder,
            "connection": q.connection, "text": text,
        });
        match ipc::call::<_, SavedQueryView>("save_query", &args).await {
            Ok(saved) => {
                state.saved_text.update(|m| {
                    m.insert(saved.id, text);
                });
                state.notice.set(Some(format!("Saved \"{}\".", saved.name)));
            }
            Err(e) => state.notice.set(Some(e.to_string())),
        }
    });
}

/// Tab `id` now shows the new saved query `q`, saved from its text.
pub fn link(id: &str, q: &SavedQueryView) {
    let state = ui();
    let text = text_of(id);
    state.tabs.update(|all| {
        if let Some(t) = all.iter_mut().find(|t| t.id == id) {
            t.saved_query = Some(q.id.clone());
            t.title.clone_from(&q.name);
        }
    });
    state.saved_text.update(|m| {
        m.insert(q.id.clone(), text);
    });
    keep(id);
    state.reload_queries();
}

/// Saved query `q` was renamed or moved: its tabs take the new name.
pub fn renamed(q: &SavedQueryView) {
    let state = ui();
    let mut changed = Vec::new();
    state.tabs.update(|all| {
        for t in all
            .iter_mut()
            .filter(|t| t.saved_query.as_ref() == Some(&q.id))
        {
            t.title.clone_from(&q.name);
            changed.push(t.id.clone());
        }
    });
    for id in changed {
        keep(&id);
    }
    state.reload_queries();
}

/// Saved query `id` is gone: its tabs keep their text as plain tabs.
pub fn unlink(id: &str) {
    let state = ui();
    let mut changed = Vec::new();
    state.tabs.update(|all| {
        for t in all
            .iter_mut()
            .filter(|t| t.saved_query.as_deref() == Some(id))
        {
            t.saved_query = None;
            changed.push(t.id.clone());
        }
    });
    state.saved_text.update(|m| {
        m.remove(id);
    });
    for tab in changed {
        keep(&tab);
    }
    state.reload_queries();
}

/// Puts `text` in the shown tab when that one is empty, and in a new tab otherwise, so no text is
/// ever replaced.
pub fn open_text(text: String) {
    let state = ui();
    let Some(connection) = state.active.get_untracked() else {
        return;
    };
    match state
        .current_untracked()
        .filter(|id| text_of(id).trim().is_empty())
    {
        Some(id) => {
            set_text(&id, text);
            ui().cursors.update(|m| {
                m.insert(id.clone(), 0);
            });
            show(id);
        }
        None => {
            let title = state.tabs.with_untracked(|t| {
                new_tab_title(
                    t.iter()
                        .filter(|t| t.connection == connection)
                        .map(|t| t.title.as_str()),
                )
            });
            leptos::task::spawn_local(async move {
                open(connection, title, text, None).await;
            });
        }
    }
}
