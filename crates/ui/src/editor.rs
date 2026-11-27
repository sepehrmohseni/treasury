//! The workspace for the open connection: toolbar, hints, the SQL editor and its results.
//!
//! The editor is a transparent textarea over highlighted text; the highlighting comes from
//! Treasury's own SQL tokenizer, so it reads a statement the way the guardrails do.

use crate::grid::Results;
use crate::home::set_setting;
use crate::ipc;
use crate::state::{Approved, Dialog, ui};
use crate::tabs;
use leptos::prelude::*;
use treasury_core::analyze::Analyzer;
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine, Environment};
use treasury_core::driver::SqlDialect;
use treasury_core::schema::{ObjectKind, ObjectNode};
use treasury_ipc::{LiveView, Prepared};
use treasury_sql::highlight::{SpanKind, highlight};
use treasury_sql::{SqlAnalyzer, render};

/// Short tips, one shown at a time.
const HINTS: &[&str] = &[
    "Ctrl+Enter runs the statement under the cursor; select text to run just that.",
    "Plain SELECTs get a 1,000-row limit. Untick \"Limit\" in the toolbar to read everything.",
    "Production connections open read-only. \"Unlock writes\" lasts 15 minutes.",
    "Double-click a connection in the list to edit it.",
    "UPDATE and DELETE on shared databases show how many rows they change before they run.",
    "Export streams straight to the file, so results larger than memory export fine.",
    "Double-click a cell of a one-table result to edit it. Nothing is saved until you review the statements.",
    "Double-click a table in the list on the left to see its rows.",
    "Every tab keeps its text as you type, so it is still there after a restart.",
    "Ctrl+S saves the tab as a query; saved queries are under Queries on the left.",
];

pub fn dialect_of(engine: Engine) -> Option<SqlDialect> {
    Some(match engine {
        Engine::Postgres => SqlDialect::Postgres,
        Engine::MySql => SqlDialect::MySql,
        Engine::MariaDb => SqlDialect::MariaDb,
        Engine::SqlServer => SqlDialect::MsSql,
        Engine::Snowflake => SqlDialect::Snowflake,
        Engine::Databricks => SqlDialect::Databricks,
        Engine::Oracle => SqlDialect::Oracle,
        Engine::Db2 => SqlDialect::Db2,
        _ => return None,
    })
}

/// The statement that shows the rows of a table, view or collection in the tree, or `None` for
/// other objects.
pub fn browse_statement(config: &ConnectionConfig, node: &ObjectNode) -> Option<String> {
    if !matches!(
        node.kind,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::MaterializedView
            | ObjectKind::Collection
    ) {
        return None;
    }
    let name = node.path.0.last()?.name.as_str();
    let database = node.path.find(ObjectKind::Database);
    let schema = node.path.find(ObjectKind::Schema);
    let d = dialect_of(config.engine);
    Some(match (config.engine, d) {
        // The command name comes first, so the text is written out rather than serialized.
        (Engine::MongoDb, _) => {
            let db = database.or(config.database.as_deref()).unwrap_or("test");
            format!(
                "{{\"find\": {}, \"filter\": {{}}, \"$db\": {}}}",
                serde_json::Value::from(name),
                serde_json::Value::from(db)
            )
        }
        (Engine::MySql | Engine::MariaDb, Some(d)) => {
            format!("SELECT * FROM {}", render::table(d, database, name))
        }
        // A table in another database needs its database named; the grid cannot edit those.
        (Engine::SqlServer, Some(d)) => match database {
            Some(db) if Some(db) != config.database.as_deref() => format!(
                "SELECT * FROM {}.{}",
                render::ident(d, db),
                render::table(d, Some(schema.unwrap_or("dbo")), name)
            ),
            _ => format!("SELECT * FROM {}", render::table(d, schema, name)),
        },
        (_, Some(d)) => format!("SELECT * FROM {}", render::table(d, schema, name)),
        (_, None) => return None,
    })
}

/// The statement to run: the selection, or the statement under the cursor.
pub fn statement_at(dialect: Option<SqlDialect>, text: &str, start: usize, end: usize) -> String {
    if end > start {
        return text.get(start..end).unwrap_or("").trim().to_owned();
    }
    let Some(d) = dialect else {
        return text.trim().to_owned();
    };
    let statements = SqlAnalyzer::new(d).classify(text);
    statements
        .iter()
        .find(|s| s.range.start <= start && start <= s.range.end + 1)
        .or(statements.last())
        .and_then(|s| text.get(s.range.clone()))
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// Character offset (what the DOM reports) to byte offset.
fn byte_offset(text: &str, chars: u32) -> usize {
    text.char_indices()
        .nth(chars as usize)
        .map_or(text.len(), |(i, _)| i)
}

fn class_of(k: SpanKind) -> &'static str {
    match k {
        SpanKind::Keyword => "tk-keyword",
        SpanKind::String => "tk-string",
        SpanKind::Number => "tk-number",
        SpanKind::Comment => "tk-comment",
        SpanKind::Placeholder => "tk-placeholder",
        _ => "",
    }
}

/// The highlighted copy of the text behind the textarea.
fn highlighted(dialect: Option<SqlDialect>, text: &str) -> AnyView {
    let Some(d) = dialect else {
        return view! { {format!("{text}\n")} }.into_any();
    };
    let mut parts = Vec::new();
    let mut at = 0;
    for span in highlight(d, text) {
        if span.range.start > at {
            parts.push(view! { <span>{text[at..span.range.start].to_owned()}</span> }.into_any());
        }
        let piece = text.get(span.range.clone()).unwrap_or("").to_owned();
        parts.push(view! { <span class=class_of(span.kind)>{piece}</span> }.into_any());
        at = span.range.end.max(at);
    }
    if at < text.len() {
        parts.push(view! { <span>{text[at..].to_owned()}</span> }.into_any());
    }
    parts.push(view! { "\n" }.into_any());
    parts.into_iter().collect_view().into_any()
}

/// Starts `statement` in tab `tab`: asks the guardrails first, then runs it or opens their
/// dialog.
pub fn start(tab: String, statement: String, limit: bool) {
    let state = ui();
    let Some(connection) = state
        .tabs
        .with_untracked(|t| t.iter().find(|t| t.id == tab).map(|t| t.connection.clone()))
    else {
        return;
    };
    if statement.is_empty() {
        return;
    }
    leptos::task::spawn_local(async move {
        let prepared = ipc::call::<_, Prepared>(
            "prepare",
            &serde_json::json!({ "id": connection, "statement": statement }),
        )
        .await;
        match prepared {
            Err(e) => state.notice.set(Some(e.to_string())),
            Ok(p) if p.refused.is_none() && p.count.is_none() && p.name_required.is_none() => {
                run(tab, connection, statement, p.token, None, limit).await;
            }
            Ok(p) => state.dialog.set(Dialog::Confirm {
                tab,
                connection,
                statement,
                prepared: p,
            }),
        }
    });
}

async fn run(
    tab: String,
    connection: ConnectionId,
    statement: String,
    token: u64,
    typed_name: Option<String>,
    limit: bool,
) {
    let state = ui();
    let Some(out) = state.outputs.with_untracked(|m| m.get(&tab).copied()) else {
        return;
    };
    if let Some(old) = out.result.get_untracked() {
        let _ = ipc::call::<_, ()>("close_result", &serde_json::json!({ "result": old })).await;
    }
    match ipc::call::<_, u64>(
        "run",
        &serde_json::json!({
            "id": connection, "statement": statement, "token": token,
            "typedName": typed_name, "limit": limit,
        }),
    )
    .await
    {
        Ok(rid) => {
            out.statement.set(Some((connection, statement)));
            out.result.set(Some(rid));
        }
        Err(e) => state.notice.set(Some(e.to_string())),
    }
}

/// The DOM's offset of the caret in `el`.
fn caret(el: &web_sys::HtmlTextAreaElement) -> u32 {
    el.selection_start().ok().flatten().unwrap_or(0)
}

#[component]
pub fn Workspace() -> impl IntoView {
    let state = ui();
    let limit = RwSignal::new(true);
    let hint = RwSignal::new((js_sys::Math::random() * HINTS.len() as f64) as usize % HINTS.len());
    let area = NodeRef::<leptos::html::Textarea>::new();

    let config = move || state.active_config();
    let live = move || {
        state
            .active
            .get()
            .and_then(|id| state.live.with(|m| m.get(&id).cloned()))
    };
    let dialect = move || config().and_then(|c| dialect_of(c.engine));
    let current = move || state.current();
    let text = move || {
        current()
            .and_then(|t| state.tab_text.with(|m| m.get(&t).cloned()))
            .unwrap_or_default()
    };
    let output = move || current().and_then(|t| state.outputs.with(|m| m.get(&t).copied()));

    // The active connection always has a tab to show.
    Effect::new(move |_| tabs::ensure_tab());

    // A tab just shown gets the focus, with the caret where it was left.
    Effect::new(move |_| {
        let Some(id) = current() else {
            return;
        };
        let at = state
            .cursors
            .with_untracked(|m| m.get(&id).copied())
            .unwrap_or(0);
        request_animation_frame(move || {
            if let Some(el) = area.get_untracked() {
                let _ = el.focus();
                let _ = el.set_selection_range(at, at);
            }
        });
    });

    let run_current = move || {
        let (Some(id), Some(el)) = (state.current(), area.get_untracked()) else {
            return;
        };
        let value = el.value();
        let s = el.selection_start().ok().flatten().unwrap_or(0);
        let e = el.selection_end().ok().flatten().unwrap_or(s);
        let statement = statement_at(
            config().and_then(|c| dialect_of(c.engine)),
            &value,
            byte_offset(&value, s),
            byte_offset(&value, e),
        );
        // A new run clears old messages; the run after saved edits keeps "Saved".
        state.notice.set(None);
        start(id, statement, limit.get_untracked());
    };

    // A table double-clicked in the tree: its rows, in a tab of their own unless one already
    // holds that statement.
    Effect::new(move |_| {
        let Some((title, statement)) = state.open_rows.get() else {
            return;
        };
        state.open_rows.set(None);
        let Some(connection) = state.active.get_untracked() else {
            return;
        };
        let existing = state.tabs.with_untracked(|t| {
            t.iter()
                .find(|t| t.connection == connection && tabs::text_of(&t.id) == statement)
                .map(|t| t.id.clone())
        });
        state.notice.set(None);
        let l = limit.get_untracked();
        match existing {
            Some(id) => {
                tabs::show(id.clone());
                start(id, statement, l);
            }
            None => leptos::task::spawn_local(async move {
                if let Some(id) = tabs::open(connection, title, statement.clone(), None).await {
                    start(id, statement, l);
                }
            }),
        }
    });

    // A statement the guardrail dialog approved.
    Effect::new(move |_| {
        if let Some(Approved {
            tab,
            connection,
            statement,
            token,
            typed_name,
        }) = state.approved.get()
        {
            state.approved.set(None);
            state.notice.set(None);
            let l = limit.get_untracked();
            leptos::task::spawn_local(run(tab, connection, statement, token, typed_name, l));
        }
    });

    let toggle_writes = move |_| {
        let Some(id) = state.active.get_untracked() else {
            return;
        };
        let unlock = live().is_some_and(|l| l.read_only);
        leptos::task::spawn_local(async move {
            let cmd = if unlock {
                "unlock_writes"
            } else {
                "lock_writes"
            };
            match ipc::call::<_, LiveView>(cmd, &serde_json::json!({ "id": id })).await {
                Ok(l) => state.live.update(|m| {
                    m.insert(id, l);
                }),
                Err(e) => state.notice.set(Some(e.to_string())),
            }
        });
    };

    let cancel = move |_| {
        if let Some(rid) = output().and_then(|o| o.result.get_untracked()) {
            leptos::task::spawn_local(async move {
                let _ = ipc::call::<_, ()>("cancel", &serde_json::json!({ "result": rid })).await;
            });
        }
    };

    // The caret moved: remember it for when the tab is shown again.
    let track_caret = move || {
        if let (Some(id), Some(el)) = (state.current(), area.get_untracked()) {
            tabs::set_cursor(&id, caret(&el));
        }
    };

    view! {
        <main
            class="workspace"
            class:production-strip=move || config().is_some_and(|c| c.environment == Environment::Production)
        >
            <div>
                <div class="toolbar">
                    <span class="title">{move || config().map(|c| c.name).unwrap_or_default()}</span>
                    {move || config().filter(|c| c.environment == Environment::Production).map(|_| view! {
                        <span class="badge" style:color="var(--red)" style:border-color="var(--red)">"Production"</span>
                    })}
                    {move || live().map(|l| if l.read_only {
                        view! { <span class="badge ro">"Read-only"</span> }.into_any()
                    } else {
                        view! { <span class="badge rw">{match l.unlocked_secs {
                            Some(s) => format!("Writes unlocked, {} min left", s.div_ceil(60)),
                            None => "Writes allowed".to_owned(),
                        }}</span> }.into_any()
                    })}
                    <span class="spacer"></span>
                    {move || {
                        let connecting = state.connecting.get().is_some() && state.connecting.get() == state.active.get();
                        connecting.then(|| view! { <span class="detail">"Connecting…"</span> })
                    }}
                    <label class="detail"><input type="checkbox" prop:checked=move || limit.get()
                        on:change=move |ev| limit.set(event_target_checked(&ev)) />" Limit"</label>
                    {move || live().filter(|_| config().is_some_and(|c| c.read_only || c.environment == Environment::Production)).map(|l| view! {
                        <button class="btn small" on:click=toggle_writes>
                            {if l.read_only { "Unlock writes" } else { "Lock writes" }}
                        </button>
                    })}
                    <button class="btn small save-query" on:click=move |_| tabs::save_current()
                        disabled=move || current().is_none() title="Ctrl+S">"Save query"</button>
                    <button class="btn small" on:click=cancel
                        disabled=move || output().is_none_or(|o| o.result.get().is_none())>"Cancel"</button>
                    <button class="btn small primary" on:click=move |_| run_current()
                        disabled=move || live().is_none() title="Ctrl+Enter">"Run"</button>
                </div>
                <Show when=move || state.hints.get()>
                    <div class="hint" role="note">
                        <span>{move || HINTS[hint.get()]}</span>
                        <span class="push">
                            <button class="btn small" on:click=move |_| hint.update(|h| *h = (*h + 1) % HINTS.len())>"Next tip"</button>
                            <button class="btn small no-tips" on:click=move |_| {
                                state.hints.set(false);
                                set_setting("hints", serde_json::Value::Bool(false));
                            }>"Don't show tips"</button>
                        </span>
                    </div>
                </Show>
                <TabStrip />
            </div>
            <div class="editor">
                <pre aria-hidden="true">{move || highlighted(dialect(), &text())}</pre>
                <textarea
                    node_ref=area
                    spellcheck="false"
                    aria-label="Statement editor"
                    placeholder="SELECT * FROM …"
                    prop:value=text
                    on:input=move |ev| {
                        if let Some(id) = state.current() {
                            tabs::set_text(&id, event_target_value(&ev));
                        }
                        track_caret();
                    }
                    on:keyup=move |_| track_caret()
                    on:click=move |_| track_caret()
                    on:blur=move |_| if let Some(id) = state.current() { tabs::keep(&id) }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        let ctrl = ev.ctrl_key() || ev.meta_key();
                        if ctrl && ev.key() == "Enter" {
                            ev.prevent_default();
                            run_current();
                        } else if ctrl && ev.key().eq_ignore_ascii_case("s") {
                            ev.prevent_default();
                            tabs::save_current();
                        }
                    }
                ></textarea>
            </div>
            <div class="panes">
                // Every tab keeps its result, and its unsaved grid edits, while another is shown.
                <For
                    each=move || state.tabs.get()
                    key=|t| t.id.clone()
                    children=move |t| {
                        let id = t.id.clone();
                        let shown = id.clone();
                        let out = state.outputs.with_untracked(|m| m.get(&id).copied()).unwrap_or_else(crate::state::Output::new);
                        // Saved grid edits: run the statement again to show the rows as they are now.
                        let rerun = Callback::new(move |()| {
                            if let Some((_, statement)) = out.statement.get_untracked() {
                                start(id.clone(), statement, limit.get_untracked());
                            }
                        });
                        view! {
                            <div class="pane" hidden=move || current().as_ref() != Some(&shown)>
                                <Results result=out.result pending=out.pending on_saved=rerun />
                            </div>
                        }
                    }
                />
            </div>
        </main>
    }
}

/// The tabs of the active connection.
#[component]
fn TabStrip() -> impl IntoView {
    let state = ui();
    let tabs_here = move || {
        let active = state.active.get();
        state.tabs.with(|t| {
            t.iter()
                .filter(|t| Some(&t.connection) == active.as_ref())
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        })
    };
    view! {
        <div class="tab-strip" role="tablist" aria-label="Editor tabs">
            <For
                each=tabs_here
                key=|id| id.clone()
                children=move |id| {
                    let (on, show, close) = (id.clone(), id.clone(), id.clone());
                    let title = {
                        let id = id.clone();
                        move || state.tabs.with(|t| t.iter().find(|t| t.id == id).map(|t| t.title.clone())).unwrap_or_default()
                    };
                    // A tab showing a saved query is marked once its text differs from what was saved.
                    let changed = {
                        let id = id.clone();
                        move || {
                            let q = state.tabs.with(|t| t.iter().find(|t| t.id == id).and_then(|t| t.saved_query.clone()));
                            q.is_some_and(|q| {
                                let text = state.tab_text.with(|m| m.get(&id).cloned());
                                state.saved_text.with(|m| m.get(&q).is_some_and(|saved| Some(saved) != text.as_ref()))
                            })
                        }
                    };
                    let label = title.clone();
                    let hover = title.clone();
                    view! {
                        <div class="etab" class:on=move || state.current().as_ref() == Some(&on)>
                            <button class="etab-title" role="tab"
                                aria-selected=move || (state.current().as_ref() == Some(&id)).to_string()
                                title=hover
                                on:click=move |_| tabs::show(show.clone())>
                                {title}
                                <Show when=changed.clone()>
                                    <span class="changed-dot" title="Changed since it was saved" aria-label="changed">"●"</span>
                                </Show>
                            </button>
                            <button class="etab-close" aria-label=move || format!("Close {}", label())
                                on:click=move |_| tabs::request_close(close.clone())>"×"</button>
                        </div>
                    }
                }
            />
            <button class="etab-new" aria-label="New tab" title="New tab" on:click=move |_| tabs::new_tab()>"+"</button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statement_under_the_cursor() {
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3";
        let second = text.find("SELECT 2").unwrap() + 3;
        assert_eq!(
            statement_at(Some(SqlDialect::Postgres), text, second, second),
            "SELECT 2"
        );
        assert_eq!(
            statement_at(Some(SqlDialect::Postgres), text, text.len(), text.len()),
            "SELECT 3"
        );
        assert_eq!(
            statement_at(Some(SqlDialect::Postgres), text, 0, 8),
            "SELECT 1"
        );
        assert_eq!(statement_at(None, " {\"ping\": 1} ", 0, 0), "{\"ping\": 1}");
    }

    fn node(
        engine: Engine,
        database: Option<&str>,
        parts: &[(ObjectKind, &str)],
    ) -> (ConnectionConfig, ObjectNode) {
        let mut config = ConnectionConfig::new("c", engine, "h").unwrap();
        config.database = database.map(str::to_owned);
        let mut path = treasury_core::schema::ObjectPath::root();
        for (k, n) in parts {
            path = path.child(*k, *n);
        }
        let (kind, name) = parts[parts.len() - 1];
        let node = ObjectNode {
            path,
            kind,
            name: name.to_owned(),
            expandable: false,
            detail: None,
        };
        (config, node)
    }

    #[test]
    fn tables_in_the_tree_open_as_a_select() {
        use ObjectKind::{Collection, Database, Group, Schema, Table, View};
        let (c, n) = node(
            Engine::Postgres,
            Some("shop"),
            &[(Schema, "Sales"), (Group, "Tables"), (Table, "order lines")],
        );
        assert_eq!(
            browse_statement(&c, &n).unwrap(),
            r#"SELECT * FROM "Sales"."order lines""#
        );
        let (c, n) = node(
            Engine::MySql,
            None,
            &[(Database, "shop"), (Group, "Views"), (View, "open")],
        );
        assert_eq!(
            browse_statement(&c, &n).unwrap(),
            "SELECT * FROM `shop`.`open`"
        );
        let (c, n) = node(
            Engine::SqlServer,
            Some("shop"),
            &[(Database, "shop"), (Schema, "dbo"), (Table, "t")],
        );
        assert_eq!(browse_statement(&c, &n).unwrap(), "SELECT * FROM [dbo].[t]");
        let (c, n) = node(
            Engine::SqlServer,
            Some("shop"),
            &[(Database, "audit"), (Schema, "dbo"), (Table, "t")],
        );
        assert_eq!(
            browse_statement(&c, &n).unwrap(),
            "SELECT * FROM [audit].[dbo].[t]"
        );
        let (c, n) = node(
            Engine::MongoDb,
            Some("app"),
            &[(Database, "app"), (Collection, "users")],
        );
        assert_eq!(
            browse_statement(&c, &n).unwrap(),
            r#"{"find": "users", "filter": {}, "$db": "app"}"#
        );
        let (c, n) = node(
            Engine::Postgres,
            None,
            &[(Schema, "public"), (Schema, "public")],
        );
        assert!(browse_statement(&c, &n).is_none(), "a schema has no rows");
    }

    #[test]
    fn character_offsets_become_byte_offsets() {
        assert_eq!(byte_offset("héllo", 2), 3);
        assert_eq!(byte_offset("abc", 10), 3);
    }
}
