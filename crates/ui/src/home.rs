//! What the window shows with no connection open: the two ways to start, and a short walkthrough
//! the first time, which can be put away for good.

use crate::ipc;
use crate::sidebar::{add_connection, import_connections};
use crate::state::ui;
use leptos::prelude::*;
use treasury_ipc::Setting;

/// The walkthrough steps: short, in the order a first session goes.
const STEPS: &[&str] = &[
    "Add a connection, or import the ones you have in another client: DBeaver, Beekeeper Studio, pgAdmin, DataGrip or MongoDB Compass.",
    "Open it from the list on the left. Production connections open read-only and are marked in red.",
    "Write a statement and press Ctrl+Enter to run the one under the cursor. Plain SELECTs get a 1,000-row limit.",
    "Before an UPDATE or DELETE runs on a shared database, Treasury shows how many rows it changes.",
    "Export a result from the bar under the grid, or double-click a cell of a one-table result to edit it.",
];

/// Saves setting `key`; if that fails, the status bar says so.
pub fn set_setting(key: Setting, value: serde_json::Value) {
    leptos::task::spawn_local(async move {
        if let Err(e) = ipc::call::<_, ()>(
            "set_setting",
            &serde_json::json!({ "key": key, "value": value }),
        )
        .await
        {
            ui().notice
                .set(Some(format!("Treasury could not save the setting: {e}")));
        }
    });
}

/// Setting `key` as saved, or `None` when it was never saved or cannot be read.
pub async fn setting(key: Setting) -> Option<serde_json::Value> {
    ipc::call::<_, Option<serde_json::Value>>("setting", &serde_json::json!({ "key": key }))
        .await
        .ok()
        .flatten()
}

#[component]
pub fn Home() -> impl IntoView {
    let state = ui();
    view! {
        <main class="home-pane">
            <div class="home">
                <Show when=move || state.connections.with(Vec::is_empty) fallback=|| view! {
                    <h1>"Open a connection"</h1>
                    <p>"Click a connection on the left. Its databases, schemas and tables open under it, and the editor opens here: write a statement and press Run, or Ctrl+Enter. Double-click a table to see its rows."</p>
                }>
                    <h1>"Connect to a database"</h1>
                </Show>
                <p>"Treasury opens PostgreSQL, MySQL, MariaDB, SQL Server, MongoDB, Redis and Valkey. Passwords go to your system keychain, never to a settings file."</p>
                <div class="actions">
                    <button class="btn primary" on:click=move |_| add_connection()>"Add connection"</button>
                    <button class="btn" on:click=move |_| import_connections()>"Import connections"</button>
                </div>
                <Show when=move || !state.tutorial_done.get()>
                    <section class="tour" aria-labelledby="tour-title">
                        <strong id="tour-title">"Your first five minutes"</strong>
                        <ol>{STEPS.iter().map(|s| view! { <li>{*s}</li> }).collect_view()}</ol>
                        <button class="btn small hide-tour" on:click=move |_| {
                            state.tutorial_done.set(true);
                            set_setting(Setting::TutorialDone, serde_json::Value::Bool(true));
                        }>"Got it, hide this"</button>
                    </section>
                </Show>
            </div>
        </main>
    }
}
