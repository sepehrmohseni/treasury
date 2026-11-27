//! The dialogs: connection form, import, guardrail confirmation, reviewing grid edits, SSH host
//! key.

use crate::ipc;
use crate::state::{Approved, Dialog, ui};
use leptos::prelude::*;
use treasury_core::config::{ConnectionConfig, Endpoint, Engine, Environment, SecretSlot, TlsMode};
use treasury_ipc::{ImportPreview, LiveView, Prepared, RowEdits, SavedQueryView, SecretInput};

/// The engines this version connects to.
const ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::SqlServer,
    Engine::MongoDb,
];

fn close() {
    ui().dialog.set(Dialog::None);
}

#[component]
pub fn Dialogs() -> impl IntoView {
    let state = ui();
    move || match state.dialog.get() {
        Dialog::None => ().into_any(),
        Dialog::Connection(c) => view! { <ConnectionForm initial=*c /> }.into_any(),
        Dialog::Import { path, preview } => {
            view! { <ImportDialog path=path preview=preview /> }.into_any()
        }
        Dialog::Confirm {
            tab,
            connection,
            statement,
            prepared,
        } => view! {
            <ConfirmDialog tab=tab connection=connection statement=statement prepared=prepared />
        }
        .into_any(),
        Dialog::SaveQuery { tab, query } => {
            view! { <SaveQueryDialog tab=tab query=query /> }.into_any()
        }
        Dialog::DeleteQuery(q) => view! { <DeleteQueryDialog query=q /> }.into_any(),
        Dialog::CloseTab {
            tab,
            unsaved_text,
            pending,
        } => view! { <CloseTabDialog tab=tab unsaved_text=unsaved_text pending=pending /> }
            .into_any(),
        Dialog::Settings => view! { <SettingsDialog /> }.into_any(),
        Dialog::ReviewEdits {
            result,
            set,
            statements,
            edits,
        } => view! {
            <ReviewDialog result=result set=set statements=statements edits=edits />
        }
        .into_any(),
        Dialog::HostKey(q) => {
            let answer = move |accept: bool| {
                let id = q.id;
                leptos::task::spawn_local(async move {
                    let _ = ipc::call::<_, ()>(
                        "host_key_answer",
                        &serde_json::json!({ "question": id, "accept": accept }),
                    )
                    .await;
                });
                close();
            };
            view! {
                <div class="backdrop">
                    <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="hk-title">
                        <header id="hk-title">"Unknown SSH host"</header>
                        <div class="body">
                            <p>{format!("Treasury has not connected to {}:{} before. Check that this fingerprint is the server's before trusting it.", q.host, q.port)}</p>
                            <div class="fingerprint">{format!("{} {}", q.algorithm, q.fingerprint)}</div>
                        </div>
                        <footer>
                            <button class="btn" on:click=move |_| answer(false)>"Refuse"</button>
                            <button class="btn primary" on:click=move |_| answer(true)>"Trust and connect"</button>
                        </footer>
                    </div>
                </div>
            }.into_any()
        }
    }
}

fn port_of(c: &ConnectionConfig) -> u16 {
    match &c.endpoint {
        Endpoint::Host { port, .. } => *port,
        _ => c.engine.default_port(),
    }
}

fn host_of(c: &ConnectionConfig) -> String {
    match &c.endpoint {
        Endpoint::Host { host, .. } => host.clone(),
        _ => String::new(),
    }
}

#[component]
fn ConnectionForm(initial: ConnectionConfig) -> impl IntoView {
    let state = ui();
    let existing = state
        .connections
        .with_untracked(|l| l.iter().any(|c| c.config.id == initial.id));
    let has_password = state.connections.with_untracked(|l| {
        l.iter()
            .find(|c| c.config.id == initial.id)
            .is_some_and(|c| c.secrets.contains(&SecretSlot::Password))
    });
    let cfg = RwSignal::new(initial);
    let password = RwSignal::new(String::new());
    let uri = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);

    let paste = move |text: String| {
        uri.set(text.clone());
        match treasury_core::uri::parse(&text) {
            Ok(p) => {
                cfg.update(|c| {
                    if let Some(e) = p.engine {
                        c.engine = e;
                    }
                    if let Some(ep) = p.endpoint.clone() {
                        c.endpoint = ep;
                    }
                    if p.database.is_some() {
                        c.database = p.database.clone();
                    }
                    if p.user.is_some() {
                        c.user = p.user.clone();
                    }
                    if let Some(m) = p.tls {
                        c.tls.mode = m;
                    }
                    c.tls.accept_invalid = p.accept_invalid_certs;
                    for (k, v) in &p.options {
                        c.options.insert(k.clone(), v.clone());
                    }
                });
                if let Some(pw) = &p.password {
                    use secrecy::ExposeSecret;
                    password.set(pw.expose_secret().to_owned());
                }
                error.set(None);
            }
            Err(e) => error.set(Some(e.to_string())),
        }
    };

    let save = move |connect: bool| {
        let mut c = cfg.get_untracked();
        if c.name.trim().is_empty() {
            c.name = match &c.endpoint {
                Endpoint::Host { host, .. } => match &c.database {
                    Some(db) => format!("{host}/{db}"),
                    None => host.clone(),
                },
                _ => c.engine.display_name().to_owned(),
            };
        }
        if let Err(e) = c.validate() {
            error.set(Some(e.to_string()));
            return;
        }
        let secrets = vec![SecretInput {
            slot: SecretSlot::Password,
            value: password.get_untracked(),
        }];
        let id = c.id.clone();
        leptos::task::spawn_local(async move {
            match ipc::call::<_, ()>(
                "save_connection",
                &serde_json::json!({ "config": c, "secrets": secrets }),
            )
            .await
            {
                Ok(()) => {
                    state.reload_connections();
                    close();
                    // Reconnect with the new settings.
                    state.live.update(|m| {
                        m.remove(&id);
                    });
                    if connect {
                        crate::sidebar::open(id);
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let delete = move |_| {
        let id = cfg.get_untracked().id;
        leptos::task::spawn_local(async move {
            match ipc::call::<_, ()>("delete_connection", &serde_json::json!({ "id": id })).await {
                Ok(()) => {
                    if state.active.get_untracked().as_ref() == Some(&id) {
                        state.active.set(None);
                    }
                    state.reload_connections();
                    close();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <div class="backdrop">
            <form class="dialog" role="dialog" aria-modal="true" aria-labelledby="cf-title"
                on:submit=move |ev| { ev.prevent_default(); save(true); }>
                <header id="cf-title">{if existing { "Edit connection" } else { "Add connection" }}</header>
                <div class="body">
                    <div class="field">
                        <label for="cf-uri">"Connection string"</label>
                        <input id="cf-uri" placeholder="postgres://user@host:5432/db" prop:value=move || uri.get()
                            on:change=move |ev| paste(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <label for="cf-engine">"Engine"</label>
                        <select id="cf-engine" on:change=move |ev| {
                            let v = event_target_value(&ev);
                            if let Some(e) = ENGINES.iter().find(|e| e.display_name() == v) {
                                cfg.update(|c| {
                                    let host = host_of(c);
                                    c.engine = *e;
                                    c.endpoint = Endpoint::Host { host, port: e.default_port() };
                                });
                            }
                        }>
                            {ENGINES.iter().map(|e| view! {
                                <option selected=move || cfg.with(|c| c.engine == *e)>{e.display_name()}</option>
                            }).collect_view()}
                        </select>
                    </div>
                    <div class="field">
                        <label for="cf-host">"Host and port"</label>
                        <div class="row2">
                            <input id="cf-host" required prop:value=move || cfg.with(host_of)
                                on:input=move |ev| cfg.update(|c| c.endpoint = Endpoint::Host { host: event_target_value(&ev), port: port_of(c) }) />
                            <input aria-label="Port" type="number" min="1" max="65535" prop:value=move || cfg.with(port_of).to_string()
                                on:input=move |ev| {
                                    let p = event_target_value(&ev).parse().unwrap_or(0);
                                    cfg.update(|c| c.endpoint = Endpoint::Host { host: host_of(c), port: p });
                                } />
                        </div>
                    </div>
                    <div class="field">
                        <label for="cf-db">"Database"</label>
                        <input id="cf-db" prop:value=move || cfg.with(|c| c.database.clone().unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); cfg.update(|c| c.database = (!v.is_empty()).then_some(v)); } />
                    </div>
                    <div class="field">
                        <label for="cf-user">"User"</label>
                        <input id="cf-user" autocomplete="off" prop:value=move || cfg.with(|c| c.user.clone().unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); cfg.update(|c| c.user = (!v.is_empty()).then_some(v)); } />
                    </div>
                    <div class="field">
                        <label for="cf-pw">"Password"</label>
                        <input id="cf-pw" type="password" autocomplete="new-password"
                            placeholder=if has_password { "Saved; leave empty to keep it" } else { "" }
                            prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <label for="cf-name">"Name"</label>
                        <input id="cf-name" placeholder="Shown in the list" prop:value=move || cfg.with(|c| c.name.clone())
                            on:input=move |ev| cfg.update(|c| c.name = event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <label for="cf-env">"Environment"</label>
                        <select id="cf-env" on:change=move |ev| {
                            let env = match event_target_value(&ev).as_str() {
                                "Local" => Environment::Local,
                                "Staging" => Environment::Staging,
                                "Production" => Environment::Production,
                                _ => Environment::Development,
                            };
                            cfg.update(|c| c.environment = env);
                        }>
                            {[("Local", Environment::Local), ("Development", Environment::Development), ("Staging", Environment::Staging), ("Production", Environment::Production)]
                                .into_iter().map(|(l, e)| view! { <option selected=move || cfg.with(|c| c.environment == e)>{l}</option> }).collect_view()}
                        </select>
                    </div>
                    <div class="field">
                        <label for="cf-tls">"TLS"</label>
                        <select id="cf-tls" on:change=move |ev| {
                            let m = match event_target_value(&ev).as_str() {
                                "Off" => TlsMode::Disable,
                                "When offered" => TlsMode::Prefer,
                                _ => TlsMode::Require,
                            };
                            cfg.update(|c| c.tls.mode = m);
                        }>
                            {[("Required, certificate checked", TlsMode::Require), ("When offered", TlsMode::Prefer), ("Off", TlsMode::Disable)]
                                .into_iter().map(|(l, m)| view! { <option selected=move || cfg.with(|c| c.tls.mode == m)>{l}</option> }).collect_view()}
                        </select>
                    </div>
                    <div class="field">
                        <span></span>
                        <label><input type="checkbox" prop:checked=move || cfg.with(|c| c.tls.accept_invalid)
                            on:change=move |ev| cfg.update(|c| c.tls.accept_invalid = event_target_checked(&ev)) />" Accept invalid certificates"</label>
                    </div>
                    <div class="field">
                        <span></span>
                        <label><input type="checkbox" prop:checked=move || cfg.with(|c| c.read_only)
                            on:change=move |ev| cfg.update(|c| c.read_only = event_target_checked(&ev)) />" Open read-only"</label>
                    </div>
                    <Show when=move || cfg.with(|c| c.tls.accept_invalid)>
                        <div class="note warn">"Certificates will not be checked, so anyone between you and the server could read and change the traffic."</div>
                    </Show>
                    <Show when=move || cfg.with(|c| c.environment == Environment::Production)>
                        <div class="note">"Production connections open read-only. Writes need \"Unlock writes\", and destructive statements need the connection name typed."</div>
                    </Show>
                    {move || error.get().map(|e| view! { <div class="note danger" role="alert">{e}</div> })}
                </div>
                <footer>
                    {existing.then(|| view! { <button type="button" class="btn danger" style:margin-inline-end="auto" on:click=delete>"Delete"</button> })}
                    <button type="button" class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button type="button" class="btn" on:click=move |_| save(false)>"Save"</button>
                    <button type="submit" class="btn primary">"Save and connect"</button>
                </footer>
            </form>
        </div>
    }
}

#[component]
fn ImportDialog(path: String, preview: ImportPreview) -> impl IntoView {
    let state = ui();
    let chosen = RwSignal::new(preview.items.iter().map(|i| i.index).collect::<Vec<_>>());
    let error = RwSignal::new(None::<String>);
    let count = preview.items.len();
    let apply = move |_| {
        let path = path.clone();
        let indices = chosen.get_untracked();
        leptos::task::spawn_local(async move {
            match ipc::call::<_, usize>(
                "import_apply",
                &serde_json::json!({ "path": path, "indices": indices }),
            )
            .await
            {
                Ok(n) => {
                    state.reload_connections();
                    state.notice.set(Some(format!(
                        "Imported {n} connection{}.",
                        if n == 1 { "" } else { "s" }
                    )));
                    close();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    view! {
        <div class="backdrop">
            <div class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="imp-title">
                <header id="imp-title">{format!("Import from {}", preview.source)}</header>
                <div class="body">
                    <p class="detail">{format!("{count} connection{} found. Passwords come along where the file has them and go to your keychain.", if count == 1 { "" } else { "s" })}</p>
                    <div class="import-list">
                        {preview.items.iter().map(|i| {
                            let idx = i.index;
                            view! {
                                <label>
                                    <input type="checkbox" prop:checked=move || chosen.with(|c| c.contains(&idx))
                                        on:change=move |ev| {
                                            let on = event_target_checked(&ev);
                                            chosen.update(|c| { c.retain(|x| *x != idx); if on { c.push(idx); } });
                                        } />
                                    <span>
                                        <div>{i.name.clone()}</div>
                                        <div class="sub">{format!("{} · {}{}", i.engine.display_name(), i.endpoint,
                                            i.database.clone().map(|d| format!(" · {d}")).unwrap_or_default())}</div>
                                        {i.notes.iter().map(|n| view! { <div class="sub">{n.clone()}</div> }).collect_view()}
                                    </span>
                                    <span class="sub">{match i.secrets { 0 => "no password".to_owned(), 1 => "1 secret".to_owned(), n => format!("{n} secrets") }}</span>
                                </label>
                            }
                        }).collect_view()}
                    </div>
                    {(!preview.skipped.is_empty()).then(|| view! {
                        <div class="note">
                            <strong>"Not imported"</strong>
                            {preview.skipped.iter().map(|(n, why)| view! { <div>{format!("{n}: {why}")}</div> }).collect_view()}
                        </div>
                    })}
                    {move || error.get().map(|e| view! { <div class="note danger" role="alert">{e}</div> })}
                </div>
                <footer>
                    <button class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button class="btn primary" on:click=apply disabled=move || chosen.with(Vec::is_empty)>
                        {move || format!("Import {}", chosen.with(Vec::len))}
                    </button>
                </footer>
            </div>
        </div>
    }
}

#[component]
fn ConfirmDialog(
    tab: String,
    connection: treasury_core::config::ConnectionId,
    statement: String,
    prepared: Prepared,
) -> impl IntoView {
    let state = ui();
    let typed = RwSignal::new(String::new());
    let name = state.connections.with_untracked(|l| {
        l.iter()
            .find(|c| c.config.id == connection)
            .map(|c| c.config.name.clone())
            .unwrap_or_default()
    });
    let needs_name = prepared.name_required.is_some();
    let refused = prepared.refused.clone();
    let read_only_refusal = refused
        .as_ref()
        .is_some_and(|r| r.contains("Unlock writes"));
    let expected = name.clone();
    let ready = move || !needs_name || typed.get().trim() == expected.trim();
    let token = prepared.token;
    let (conn_run, stmt_run) = (connection.clone(), statement.clone());
    let go = move |_| {
        state.approved.set(Some(Approved {
            tab: tab.clone(),
            connection: conn_run.clone(),
            statement: stmt_run.clone(),
            token,
            typed_name: needs_name.then(|| typed.get_untracked()),
        }));
        close();
    };
    let unlock = move |_| {
        let id = connection.clone();
        leptos::task::spawn_local(async move {
            match ipc::call::<_, LiveView>("unlock_writes", &serde_json::json!({ "id": id })).await
            {
                Ok(l) => {
                    state.live.update(|m| {
                        m.insert(id, l);
                    });
                    state.notice.set(Some(
                        "Writes unlocked for 15 minutes. Run the statement again.".into(),
                    ));
                }
                Err(e) => state.notice.set(Some(e.to_string())),
            }
            close();
        });
    };
    view! {
        <div class="backdrop">
            <div class="dialog" role="alertdialog" aria-modal="true" aria-labelledby="gd-title">
                <header id="gd-title">{if refused.is_some() { "This statement cannot run" } else { "Check before it runs" }}</header>
                <div class="body">
                    <code class="fingerprint">{prepared.summary.clone()}</code>
                    {refused.clone().map(|r| view! { <div class="note danger">{r}</div> })}
                    {prepared.count.map(|n| view! {
                        <div class="note warn">{format!("It changes {n} row{} on {name}.", if n == 1 { "" } else { "s" })}</div>
                    })}
                    {prepared.name_required.clone().map(|why| view! {
                        <div class="note danger">{why}</div>
                        <div class="field">
                            <label for="gd-name">"Type the connection name"</label>
                            <input id="gd-name" autocomplete="off" prop:value=move || typed.get()
                                on:input=move |ev| typed.set(event_target_value(&ev)) />
                        </div>
                    })}
                </div>
                <footer>
                    <button class="btn" on:click=move |_| close()>"Cancel"</button>
                    {if refused.is_some() {
                        read_only_refusal.then(|| view! { <button class="btn primary" on:click=unlock>"Unlock writes for 15 minutes"</button> }).into_any()
                    } else {
                        view! { <button class="btn primary" class:danger=needs_name on:click=go disabled=move || !ready()>"Run it"</button> }.into_any()
                    }}
                </footer>
            </div>
        </div>
    }
}

#[component]
fn ReviewDialog(
    result: u64,
    set: usize,
    statements: Vec<String>,
    edits: RowEdits,
) -> impl IntoView {
    let state = ui();
    let error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let save = move |_| {
        let edits = edits.clone();
        saving.set(true);
        leptos::task::spawn_local(async move {
            match ipc::call::<_, u64>(
                "edit_apply",
                &serde_json::json!({ "result": result, "set": set, "edits": edits }),
            )
            .await
            {
                Ok(n) => {
                    state.notice.set(Some(match n {
                        1 => "Saved. 1 row changed.".to_owned(),
                        n => format!("Saved. {} rows changed.", crate::grid::thousands(n)),
                    }));
                    state.saved.set(Some(result));
                    close();
                }
                Err(e) => {
                    saving.set(false);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };
    view! {
        <div class="backdrop">
            <div class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="rv-title">
                <header id="rv-title">"Review changes"</header>
                <div class="body">
                    <p class="detail">{format!(
                        "{} run in one transaction: all of them are saved, or none.",
                        if statements.len() == 1 { "This statement will".to_owned() } else { format!("These {} statements will", statements.len()) }
                    )}</p>
                    <pre class="statements">{statements.join("\n")}</pre>
                    {move || error.get().map(|e| view! { <div class="note danger" role="alert">{e}</div> })}
                </div>
                <footer>
                    <button class="btn" on:click=move |_| close()>"Keep editing"</button>
                    <button class="btn primary" on:click=save disabled=move || saving.get()>"Save changes"</button>
                </footer>
            </div>
        </div>
    }
}

/// Saving a tab as a new query, or renaming and moving a saved one (with deleting it).
#[component]
fn SaveQueryDialog(tab: Option<String>, query: Option<SavedQueryView>) -> impl IntoView {
    let state = ui();
    let existing = query.clone();
    let name = RwSignal::new(query.as_ref().map(|q| q.name.clone()).unwrap_or_else(|| {
        tab.as_deref()
            .and_then(|t| {
                state
                    .tabs
                    .with_untracked(|all| all.iter().find(|x| x.id == t).map(|x| x.title.clone()))
            })
            .filter(|t| !t.starts_with("Query "))
            .unwrap_or_default()
    }));
    let folder = RwSignal::new(query.as_ref().map(|q| q.folder.clone()).unwrap_or_default());
    let everywhere = RwSignal::new(query.as_ref().is_some_and(|q| q.connection.is_none()));
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let connection = tab
        .as_deref()
        .and_then(|t| {
            state
                .tabs
                .with_untracked(|all| all.iter().find(|x| x.id == t).map(|x| x.connection.clone()))
        })
        .or_else(|| query.as_ref().and_then(|q| q.connection.clone()))
        .or_else(|| state.active.get_untracked());
    let tab_for_save = tab.clone();
    let save = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        let id = existing.as_ref().map(|q| q.id.clone());
        let renaming = id.is_some();
        let text = tab_for_save.as_deref().map(crate::tabs::text_of);
        let args = serde_json::json!({
            "id": id,
            "name": name.get_untracked(),
            "folder": folder.get_untracked(),
            "connection": if everywhere.get_untracked() { None } else { connection.clone() },
            // A rename keeps the saved text.
            "text": if renaming { None } else { text },
        });
        let tab = tab_for_save.clone();
        leptos::task::spawn_local(async move {
            match ipc::call::<_, SavedQueryView>("save_query", &args).await {
                Ok(q) => {
                    match (&tab, renaming) {
                        (Some(t), false) => crate::tabs::link(t, &q),
                        _ => crate::tabs::renamed(&q),
                    }
                    state.notice.set(Some(format!("Saved \"{}\".", q.name)));
                    close();
                }
                Err(e) => {
                    busy.set(false);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };
    let delete = query.clone().map(|q| {
        view! {
            <button type="button" class="btn danger delete-query" on:click=move |_| state.dialog.set(Dialog::DeleteQuery(q.clone()))>"Delete"</button>
        }
    });
    view! {
        <div class="backdrop">
            <form class="dialog" role="dialog" aria-modal="true" aria-labelledby="sq-title" on:submit=save>
                <header id="sq-title">{if query.is_some() { "Saved query" } else { "Save query" }}</header>
                <div class="body">
                    <div class="field">
                        <label for="sq-name">"Name"</label>
                        <input id="sq-name" autocomplete="off" prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <label for="sq-folder">"Folder"</label>
                        <input id="sq-folder" autocomplete="off" placeholder="reports/monthly" prop:value=move || folder.get()
                            on:input=move |ev| folder.set(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <span></span>
                        <label><input type="checkbox" id="sq-everywhere" prop:checked=move || everywhere.get()
                            on:change=move |ev| everywhere.set(event_target_checked(&ev)) />" Show on every connection"</label>
                    </div>
                    {move || error.get().map(|e| view! { <div class="note danger" role="alert">{e}</div> })}
                </div>
                <footer>
                    {delete}
                    <button type="button" class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button type="submit" class="btn primary" disabled=move || busy.get() || name.get().trim().is_empty()>"Save"</button>
                </footer>
            </form>
        </div>
    }
}

#[component]
fn DeleteQueryDialog(query: SavedQueryView) -> impl IntoView {
    let state = ui();
    let q = query.clone();
    let delete = move |_| {
        let q = q.clone();
        leptos::task::spawn_local(async move {
            match ipc::call::<_, ()>("delete_saved_query", &serde_json::json!({ "id": q.id })).await
            {
                Ok(()) => {
                    crate::tabs::unlink(&q.id);
                    state.notice.set(Some(format!("Deleted \"{}\".", q.name)));
                }
                Err(e) => state.notice.set(Some(e.to_string())),
            }
            close();
        });
    };
    view! {
        <div class="backdrop">
            <div class="dialog" role="alertdialog" aria-modal="true" aria-labelledby="dq-title">
                <header id="dq-title">"Delete saved query"</header>
                <div class="body">
                    <p>{format!("Delete \"{}\"? This cannot be undone. Tabs that show it keep their text.", query.name)}</p>
                </div>
                <footer>
                    <button class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button class="btn danger" on:click=delete>"Delete saved query"</button>
                </footer>
            </div>
        </div>
    }
}

#[component]
fn CloseTabDialog(tab: String, unsaved_text: bool, pending: usize) -> impl IntoView {
    let state = ui();
    let title = state
        .tabs
        .with_untracked(|t| t.iter().find(|x| x.id == tab).map(|x| x.title.clone()))
        .unwrap_or_default();
    let id = tab.clone();
    let save_first = tab.clone();
    view! {
        <div class="backdrop">
            <div class="dialog" role="alertdialog" aria-modal="true" aria-labelledby="ct-title">
                <header id="ct-title">{format!("Close \"{title}\"?")}</header>
                <div class="body">
                    {unsaved_text.then(|| view! { <p>"Its text is not saved as a query, so closing the tab discards it."</p> })}
                    {(pending > 0).then(|| view! { <p>{if pending == 1 {
                        "Its result has 1 grid edit that is not saved, which is lost too.".to_owned()
                    } else {
                        format!("Its result has {pending} grid edits that are not saved, which are lost too.")
                    }}</p> })}
                </div>
                <footer>
                    <button class="btn" on:click=move |_| close()>"Keep it open"</button>
                    {unsaved_text.then(|| view! {
                        <button class="btn" on:click=move |_| {
                            crate::tabs::show(save_first.clone());
                            close();
                            crate::tabs::save_current();
                        }>"Save query"</button>
                    })}
                    <button class="btn danger close-tab" on:click=move |_| {
                        close();
                        crate::tabs::close(id.clone());
                    }>"Close tab"</button>
                </footer>
            </div>
        </div>
    }
}

/// Shows the window in `theme` ("light", "dark"), or as the system does for anything else.
pub fn apply_theme(theme: &str) {
    if let Some(root) = document().document_element() {
        let _ = match theme {
            "light" | "dark" => root.set_attribute("data-theme", theme),
            _ => root.remove_attribute("data-theme"),
        };
    }
}

/// The row limit a plain SELECT gets unless "Limit" is unticked.
const DEFAULT_ROW_LIMIT: u64 = 1_000;

#[component]
fn SettingsDialog() -> impl IntoView {
    let state = ui();
    let theme = RwSignal::new("system".to_owned());
    let row_limit = RwSignal::new(DEFAULT_ROW_LIMIT.to_string());
    let history = RwSignal::new(true);
    let update_check = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    leptos::task::spawn_local(async move {
        let get = |key: &'static str| async move {
            ipc::call::<_, Option<serde_json::Value>>("setting", &serde_json::json!({ "key": key }))
                .await
                .ok()
                .flatten()
        };
        if let Some(v) = get("theme")
            .await
            .and_then(|v| v.as_str().map(str::to_owned))
        {
            theme.set(v);
        }
        if let Some(v) = get("row_limit").await.and_then(|v| v.as_u64()) {
            row_limit.set(v.to_string());
        }
        if let Some(v) = get("history").await.and_then(|v| v.as_bool()) {
            history.set(v);
        }
        if let Some(v) = get("update_check").await.and_then(|v| v.as_bool()) {
            update_check.set(v);
        }
    });
    let save = move |_| {
        let limit = match row_limit.get_untracked().trim().parse::<u64>() {
            Ok(n) if (1..=1_000_000).contains(&n) => n,
            _ => {
                error.set(Some(
                    "The row limit is a whole number from 1 to 1,000,000.".into(),
                ));
                return;
            }
        };
        let theme = theme.get_untracked();
        apply_theme(&theme);
        crate::home::set_setting("theme", serde_json::Value::String(theme));
        crate::home::set_setting("row_limit", serde_json::Value::from(limit));
        crate::home::set_setting("history", serde_json::Value::Bool(history.get_untracked()));
        crate::home::set_setting(
            "hints",
            serde_json::Value::Bool(state.hints.get_untracked()),
        );
        crate::home::set_setting(
            "update_check",
            serde_json::Value::Bool(update_check.get_untracked()),
        );
        close();
    };
    let walkthrough = move |_| {
        state.tutorial_done.set(false);
        crate::home::set_setting("tutorial_done", serde_json::Value::Bool(false));
        state.active.set(None);
        close();
    };
    view! {
        <div class="backdrop">
            <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="st-title">
                <header id="st-title">"Settings"</header>
                <div class="body">
                    <div class="field">
                        <label for="st-theme">"Theme"</label>
                        <select id="st-theme" on:change=move |ev| theme.set(event_target_value(&ev))>
                            {[("system", "Same as the system"), ("light", "Light"), ("dark", "Dark")]
                                .into_iter()
                                .map(|(v, l)| view! { <option value=v selected=move || theme.with(|t| t == v)>{l}</option> })
                                .collect_view()}
                        </select>
                    </div>
                    <div class="field">
                        <label for="st-limit">"Row limit"</label>
                        <input id="st-limit" type="number" min="1" max="1000000"
                            prop:value=move || row_limit.get() on:input=move |ev| row_limit.set(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <span></span>
                        <label><input type="checkbox" prop:checked=move || history.get()
                            on:change=move |ev| history.set(event_target_checked(&ev)) />" Keep a history of statements run"</label>
                    </div>
                    <div class="field">
                        <span></span>
                        <label><input type="checkbox" prop:checked=move || state.hints.get()
                            on:change=move |ev| state.hints.set(event_target_checked(&ev)) />" Show tips"</label>
                    </div>
                    <div class="field">
                        <span></span>
                        <label><input type="checkbox" prop:checked=move || update_check.get()
                            on:change=move |ev| update_check.set(event_target_checked(&ev)) />" Check for a new version when Treasury starts"</label>
                    </div>
                    <div class="field">
                        <span></span>
                        <span><button class="btn small walkthrough" on:click=walkthrough>"Show the walkthrough again"</button></span>
                    </div>
                    <p class="detail">"History stays on this computer, encrypted, and production connections keep none. The version check asks GitHub which release is the latest; it carries nothing about your databases."</p>
                    {move || error.get().map(|e| view! { <div class="note danger" role="alert">{e}</div> })}
                </div>
                <footer>
                    <button class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button class="btn primary" on:click=save>"Save"</button>
                </footer>
            </div>
        </div>
    }
}
