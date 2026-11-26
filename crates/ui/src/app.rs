//! The window: sidebar, workspace or home screen, status bar, and the open dialog.

use crate::dialogs::Dialogs;
use crate::editor::Workspace;
use crate::home::Home;
use crate::ipc;
use crate::sidebar::Sidebar;
use crate::state::{Dialog, ui};
use leptos::prelude::*;
use treasury_ipc::{AppFacts, HostKeyQuestion, UpdateView};

#[component]
pub fn App() -> impl IntoView {
    let state = ui();
    leptos::task::spawn_local(async move {
        if let Ok(f) = ipc::call0::<AppFacts>("app_facts").await {
            state.facts.set(Some(f));
        }
        let get = |key: &'static str| async move {
            ipc::call::<_, Option<serde_json::Value>>("setting", &serde_json::json!({ "key": key }))
                .await
                .ok()
                .flatten()
        };
        if let Some(v) = get("hints").await.and_then(|v| v.as_bool()) {
            state.hints.set(v);
        }
        if let Some(v) = get("tutorial_done").await.and_then(|v| v.as_bool()) {
            state.tutorial_done.set(v);
        }
        if let Some(v) = get("theme")
            .await
            .and_then(|v| v.as_str().map(str::to_owned))
        {
            crate::dialogs::apply_theme(&v);
        }
        // The one request Treasury makes on its own, unless it is turned off in Settings. A
        // failed check (offline, say) is not worth interrupting anyone for.
        if get("update_check").await.and_then(|v| v.as_bool()) != Some(false)
            && let Ok(found) = ipc::call0::<Option<UpdateView>>("check_update").await
        {
            state.update.set(found);
        }
    });
    state.reload_connections();
    ipc::listen("host-key", move |event| {
        let payload = js_sys::Reflect::get(&event, &"payload".into()).unwrap_or(event);
        if let Ok(q) = serde_wasm_bindgen::from_value::<HostKeyQuestion>(payload) {
            state.dialog.set(Dialog::HostKey(q));
        }
    });

    view! {
        <div class="app">
            <Sidebar />
            <Show when=move || state.active.get().is_some() fallback=|| view! { <Home /> }>
                <Workspace />
            </Show>
            <StatusBar />
            <Dialogs />
        </div>
    }
}

#[component]
fn StatusBar() -> impl IntoView {
    let state = ui();
    let live = move || {
        state
            .active
            .get()
            .and_then(|id| state.live.with(|m| m.get(&id).cloned()))
    };
    view! {
        <footer class="statusbar" role="status">
            {move || match (state.active_config(), live()) {
                (Some(c), Some(l)) => view! {
                    <span>{c.name.clone()}</span>
                    <span>{format!("{} {}", l.info.product, l.info.version)}</span>
                    <span>{if l.read_only { "Read-only" } else { "Writes allowed" }}</span>
                }.into_any(),
                (Some(c), None) => view! { <span>{c.name.clone()}</span><span>"Not connected"</span> }.into_any(),
                _ => view! { <span>"No connection open"</span> }.into_any(),
            }}
            <span class="push">
                {move || state.notice.get().unwrap_or_default()}
            </span>
            {move || state.update.get().map(|u| {
                let version = u.version.clone();
                let install = move |_| {
                    state.notice.set(Some(format!("Downloading Treasury {version}…")));
                    leptos::task::spawn_local(async move {
                        if let Err(e) = ipc::call0::<()>("install_update").await {
                            state.notice.set(Some(e.to_string()));
                        }
                    });
                };
                view! {
                    <span class="update" title=u.notes.clone().unwrap_or_default()>
                        {format!("Treasury {} is available", u.version)}
                        <button class="btn small primary" on:click=install>"Install and restart"</button>
                        <button class="btn small" on:click=move |_| state.update.set(None)>"Not now"</button>
                    </span>
                }
            })}
            {move || state.facts.get().filter(|f| f.secrets_in_memory).map(|_| view! {
                <span title="No system keychain was found">"Passwords are kept only until Treasury quits"</span>
            })}
            <span>{move || state.facts.get().map(|f| format!("Treasury {}", f.version)).unwrap_or_default()}</span>
        </footer>
    }
}
