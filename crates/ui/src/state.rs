//! The interface's state, shared through Leptos context.

use leptos::prelude::*;
use std::collections::HashMap;
use treasury_core::config::{ConnectionConfig, ConnectionId};
use treasury_ipc::{
    AppFacts, ConnectionView, HostKeyQuestion, ImportPreview, LiveView, Prepared, RowEdits,
    UpdateView,
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
    /// What the guardrails need before a statement runs.
    Confirm {
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
    /// The editor text of each connection.
    pub texts: RwSignal<HashMap<ConnectionId, String>>,
    /// Asks the workspace to run a statement the confirm dialog approved.
    pub approved: RwSignal<Option<Approved>>,
    /// The result whose edits the review dialog just saved, so it reads its rows again.
    pub saved: RwSignal<Option<u64>>,
    /// A newer release, found by the check at start.
    pub update: RwSignal<Option<UpdateView>>,
    /// A statement for the workspace to put in the editor and run, such as the rows of a table
    /// double-clicked in the tree.
    pub open_rows: RwSignal<Option<String>>,
}

/// A statement the user confirmed in the guardrail dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct Approved {
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
            texts: RwSignal::new(HashMap::new()),
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
