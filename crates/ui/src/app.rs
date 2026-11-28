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
        if let Some(v) = crate::home::setting(treasury_ipc::Setting::Hints)
            .await
            .and_then(|v| v.as_bool())
        {
            state.hints.set(v);
        }
        if let Some(v) = crate::home::setting(treasury_ipc::Setting::TutorialDone)
            .await
            .and_then(|v| v.as_bool())
        {
            state.tutorial_done.set(v);
        }
        if let Some(v) = crate::home::setting(treasury_ipc::Setting::SideWidth)
            .await
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v.clamp(200, 640)).ok())
        {
            state.side_width.set(v);
        }
        if let Some(v) = crate::home::setting(treasury_ipc::Setting::Theme)
            .await
            .and_then(|v| v.as_str().map(str::to_owned))
        {
            crate::dialogs::apply_theme(&v);
        }
        // The one request Treasury makes on its own, unless it is turned off in Settings: at
        // start, then every few hours while it stays open.
        background_check().await;
    });
    let _ = set_interval_with_handle(
        || leptos::task::spawn_local(background_check()),
        UPDATE_EVERY,
    );
    state.reload_connections();
    crate::tabs::load();
    // Ctrl+K (Cmd+K) opens the command palette from anywhere, and closes it again.
    let _ = window_event_listener(leptos::ev::keydown, move |ev| {
        if (ev.ctrl_key() || ev.meta_key()) && ev.key().eq_ignore_ascii_case("k") {
            ev.prevent_default();
            match state.dialog.get_untracked() {
                Dialog::Palette => state.dialog.set(Dialog::None),
                Dialog::None => state.dialog.set(Dialog::Palette),
                // Another dialog is waiting for an answer.
                _ => {}
            }
        }
    });
    ipc::listen("host-key", move |event| {
        let payload = js_sys::Reflect::get(&event, &"payload".into()).unwrap_or(event);
        if let Ok(q) = serde_wasm_bindgen::from_value::<HostKeyQuestion>(payload) {
            state.dialog.set(Dialog::HostKey(q));
        }
    });

    view! {
        <div class="app" style:--side=move || format!("{}px", state.side_width.get())>
            <Sidebar />
            <Show when=move || state.active.get().is_some() fallback=|| view! { <Home /> }>
                <Workspace />
            </Show>
            <StatusBar />
            <Dialogs />
            <crate::navigator::ContextMenu />
        </div>
    }
}

/// How often an open Treasury asks again whether a newer release exists.
const UPDATE_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Asks GitHub for a newer release unless Settings turned that off. A failed check (offline,
/// say) is not worth interrupting anyone for; an update found shows in the status bar.
async fn background_check() {
    let state = ui();
    let enabled = crate::home::setting(treasury_ipc::Setting::UpdateCheck)
        .await
        .and_then(|v| v.as_bool())
        != Some(false);
    if enabled && let Ok(found) = check_for_update().await {
        state.update.set(found);
    }
}

/// Asks GitHub whether a release newer than this one exists.
pub async fn check_for_update() -> Result<Option<UpdateView>, String> {
    ipc::call0::<Option<UpdateView>>("check_update")
        .await
        .map_err(|e| e.to_string())
}

/// Downloads `version`, checks its signature, installs it and restarts Treasury.
pub fn install_update(version: &str) {
    let state = ui();
    state
        .notice
        .set(Some(format!("Downloading Treasury {version}…")));
    leptos::task::spawn_local(async move {
        if let Err(e) = ipc::call0::<()>("install_update").await {
            state.notice.set(Some(e.to_string()));
        }
    });
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
                let install = move |_| install_update(&version);
                view! {
                    <span class="update" title=u.notes.clone().unwrap_or_default()>
                        {format!("Treasury {} is available", u.version)}
                        <button class="btn small primary" on:click=install>"Install and restart"</button>
                        <button class="btn small" on:click=move |_| state.update.set(None)>"Not now"</button>
                    </span>
                }
            })}
            {move || state.facts.get().filter(|f| f.secrets_in_memory).map(|_| view! {
                <span title="No system keychain was found">"Passwords, history and tabs are kept only until Treasury quits"</span>
            })}
            <span>{move || state.facts.get().map(|f| format!("Treasury {}", f.version)).unwrap_or_default()}</span>
        </footer>
    }
}
