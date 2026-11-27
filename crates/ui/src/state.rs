//! The interface's state, shared by every component.

use leptos::prelude::*;
use std::collections::HashMap;
use treasury_core::config::{ConnectionConfig, ConnectionId};
use treasury_ipc::{
    AppFacts, ConnectionView, HostKeyQuestion, ImportPreview, LiveView, Prepared, RowEdits,
    SavedQueryView, UpdateView,
};

/// A dialog over the window; one at a time.
#[derive(Clone, Debug, PartialEq)]
pub enum Dialog {
    /// None open.
    None,
    /// Adding or editing a connection.
    Connection(Box<ConnectionConfig>),
    /// Choosing connections from an import file.
    Import {
        path: String,
        preview: ImportPreview,
    },
    /// What the guardrails need before a statement runs in tab `tab`.
    Confirm {
        tab: String,
        connection: ConnectionId,
        statement: String,
        prepared: Prepared,
    },
    /// An SSH host key seen for the first time.
    HostKey(HostKeyQuestion),
    /// Theme, row limit, history, tips and the walkthrough.
    Settings,
    /// The statements pending grid edits become, before they are saved.
    ReviewEdits {
        result: u64,
        set: usize,
        statements: Vec<String>,
        edits: RowEdits,
    },
    /// Saving tab `tab` as a new query, or renaming and moving `query`.
    SaveQuery {
        tab: Option<String>,
        query: Option<SavedQueryView>,
    },
    /// Deleting a saved query.
    DeleteQuery(SavedQueryView),
    /// Deleting the history of a connection.
    ClearHistory(ConnectionId),
    /// Closing a tab that holds work not saved anywhere else.
    CloseTab {
        tab: String,
        /// Its text is not saved as a query.
        unsaved_text: bool,
        /// Grid edits not saved yet.
        pending: usize,
    },
}

/// An editor tab as the strip shows it. Its text is in [`Ui::tab_text`], its result in
/// [`Ui::outputs`].
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub id: String,
    pub connection: ConnectionId,
    pub title: String,
    pub saved_query: Option<String>,
    /// Its place in the strip, kept by the app.
    pub position: u32,
}

/// Where a tab's statements put their result.
#[derive(Clone, Copy)]
pub struct Output {
    pub result: RwSignal<Option<u64>>,
    /// The statement that produced the result, to run again after its rows were edited.
    pub statement: RwSignal<Option<(ConnectionId, String)>>,
    /// Grid edits not saved yet.
    pub pending: RwSignal<usize>,
}

impl Output {
    pub fn new() -> Self {
        Self {
            result: RwSignal::new(None),
            statement: RwSignal::new(None),
            pending: RwSignal::new(0),
        }
    }
}

/// What the sidebar lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideView {
    Connections,
    Queries,
    History,
}

/// Everything the components share.
#[derive(Clone, Copy)]
pub struct Ui {
    pub facts: RwSignal<Option<AppFacts>>,
    pub connections: RwSignal<Vec<ConnectionView>>,
    pub active: RwSignal<Option<ConnectionId>>,
    pub live: RwSignal<HashMap<ConnectionId, LiveView>>,
    pub connecting: RwSignal<Option<ConnectionId>>,
    pub dialog: RwSignal<Dialog>,
    /// A message for the user that is not tied to a result.
    pub notice: RwSignal<Option<String>>,
    pub hints: RwSignal<bool>,
    pub tutorial_done: RwSignal<bool>,
    /// Editor tabs of every connection, in strip order.
    pub tabs: RwSignal<Vec<Tab>>,
    /// The tabs kept by the app have been read.
    pub tabs_loaded: RwSignal<bool>,
    /// Each tab's editor text.
    pub tab_text: RwSignal<HashMap<String, String>>,
    /// Each tab's cursor, in characters.
    pub cursors: RwSignal<HashMap<String, u32>>,
    /// The tab shown for each connection.
    pub current_tab: RwSignal<HashMap<ConnectionId, String>>,
    /// Each tab's result.
    pub outputs: RwSignal<HashMap<String, Output>>,
    /// Saved queries, without their text.
    pub queries: RwSignal<Vec<SavedQueryView>>,
    /// The text of each saved query open in a tab, as last saved, to tell a changed tab.
    pub saved_text: RwSignal<HashMap<String, String>>,
    pub side_view: RwSignal<SideView>,
    /// Changes whenever the history may have gained or lost entries.
    pub history_rev: RwSignal<u64>,
    /// Asks the workspace to run a statement the confirm dialog approved.
    pub approved: RwSignal<Option<Approved>>,
    /// The result whose edits the review dialog just saved, so it reads its rows again.
    pub saved: RwSignal<Option<u64>>,
    /// A newer release, found by the check at start.
    pub update: RwSignal<Option<UpdateView>>,
    /// A tab title and statement for the workspace to open and run, such as the rows of a table
    /// double-clicked in the tree.
    pub open_rows: RwSignal<Option<(String, String)>>,
}

/// A statement the user confirmed in the guardrail dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct Approved {
    pub tab: String,
    pub connection: ConnectionId,
    pub statement: String,
    pub token: u64,
    pub typed_name: Option<String>,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            facts: RwSignal::new(None),
            connections: RwSignal::new(Vec::new()),
            active: RwSignal::new(None),
            live: RwSignal::new(HashMap::new()),
            connecting: RwSignal::new(None),
            dialog: RwSignal::new(Dialog::None),
            notice: RwSignal::new(None),
            hints: RwSignal::new(true),
            tutorial_done: RwSignal::new(false),
            tabs: RwSignal::new(Vec::new()),
            tabs_loaded: RwSignal::new(false),
            tab_text: RwSignal::new(HashMap::new()),
            cursors: RwSignal::new(HashMap::new()),
            current_tab: RwSignal::new(HashMap::new()),
            outputs: RwSignal::new(HashMap::new()),
            queries: RwSignal::new(Vec::new()),
            saved_text: RwSignal::new(HashMap::new()),
            side_view: RwSignal::new(SideView::Connections),
            history_rev: RwSignal::new(0),
            approved: RwSignal::new(None),
            saved: RwSignal::new(None),
            update: RwSignal::new(None),
            open_rows: RwSignal::new(None),
        }
    }

    /// The active connection's saved settings.
    pub fn active_config(&self) -> Option<ConnectionConfig> {
        let id = self.active.get()?;
        self.connections.with(|list| {
            list.iter()
                .find(|c| c.config.id == id)
                .map(|c| c.config.clone())
        })
    }

    /// The tab shown for the active connection.
    pub fn current(&self) -> Option<String> {
        let id = self.active.get()?;
        self.current_tab.with(|m| m.get(&id).cloned())
    }

    /// [`Ui::current`], without subscribing to changes.
    pub fn current_untracked(&self) -> Option<String> {
        let id = self.active.get_untracked()?;
        self.current_tab.with_untracked(|m| m.get(&id).cloned())
    }

    /// Says that the history may have changed, so the history list reads it again.
    pub fn history_changed(&self) {
        self.history_rev.update(|n| *n += 1);
    }

    /// Reloads the saved query list from the app.
    pub fn reload_queries(self) {
        leptos::task::spawn_local(async move {
            match crate::ipc::call0::<Vec<SavedQueryView>>("saved_queries").await {
                Ok(list) => self.queries.set(list),
                Err(e) => self.notice.set(Some(e.to_string())),
            }
        });
    }

    /// Reloads the connection list from the app.
    pub fn reload_connections(self) {
        leptos::task::spawn_local(async move {
            match crate::ipc::call0::<Vec<ConnectionView>>("connections").await {
                Ok(list) => self.connections.set(list),
                Err(e) => self.notice.set(Some(e.to_string())),
            }
        });
    }
}

thread_local! {
    static UI: std::cell::OnceCell<Ui> = const { std::cell::OnceCell::new() };
}

/// The shared state, created on first use and kept for the life of the page. It lives outside
/// Leptos's context so event handlers and async tasks, which run without a reactive owner, reach
/// it too.
pub fn ui() -> Ui {
    UI.with(|cell| *cell.get_or_init(Ui::new))
}
