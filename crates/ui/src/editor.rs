//! The workspace for the open connection: toolbar, hints, the SQL editor and its results.
//!
//! The editor is a transparent textarea over highlighted text; the highlighting comes from
//! Treasury's own SQL tokenizer, so it reads a statement the way the guardrails do.

use crate::grid::Results;
use crate::home::set_setting;
use crate::ipc;
use crate::state::{Approved, Dialog, ui};
use leptos::prelude::*;
use treasury_core::analyze::Analyzer;
use treasury_core::config::{ConnectionId, Engine, Environment};
use treasury_core::driver::SqlDialect;
use treasury_ipc::{LiveView, Prepared};
use treasury_sql::SqlAnalyzer;
use treasury_sql::highlight::{SpanKind, highlight};

/// Short tips, one shown at a time.
const HINTS: &[&str] = &[
    "Ctrl+Enter runs the statement under the cursor; select text to run just that.",
    "Plain SELECTs get a 1,000-row limit. Untick \"Limit\" in the toolbar to read everything.",
    "Production connections open read-only. \"Unlock writes\" lasts 15 minutes.",
    "Double-click a connection in the list to edit it.",
    "UPDATE and DELETE on shared databases show how many rows they change before they run.",
    "Export streams straight to the file, so results larger than memory export fine.",
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

/// Starts `statement`: asks the guardrails first, then runs it or opens their dialog.
pub fn start(
    connection: ConnectionId,
    statement: String,
    result: RwSignal<Option<u64>>,
    limit: bool,
) {
    let state = ui();
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
                run(connection, statement, p.token, None, result, limit).await;
            }
            Ok(p) => state.dialog.set(Dialog::Confirm {
                connection,
                statement,
                prepared: p,
            }),
        }
    });
}

async fn run(
    connection: ConnectionId,
    statement: String,
    token: u64,
    typed_name: Option<String>,
    result: RwSignal<Option<u64>>,
    limit: bool,
) {
    let state = ui();
    if let Some(old) = result.get_untracked() {
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
            state.notice.set(None);
            result.set(Some(rid));
        }
        Err(e) => state.notice.set(Some(e.to_string())),
    }
}

#[component]
pub fn Workspace() -> impl IntoView {
    let state = ui();
    let result = RwSignal::new(None::<u64>);
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
    let text = move || {
        state
            .active
            .get()
            .and_then(|id| state.texts.with(|t| t.get(&id).cloned()))
            .unwrap_or_default()
    };
    let set_text = move |value: String| {
        if let Some(id) = state.active.get_untracked() {
            state.texts.update(|t| {
                t.insert(id, value);
            });
        }
    };

    let run_current = move || {
        let (Some(id), Some(el)) = (state.active.get_untracked(), area.get_untracked()) else {
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
        start(id, statement, result, limit.get_untracked());
    };

    // A statement the guardrail dialog approved.
    Effect::new(move |_| {
        if let Some(Approved {
            connection,
            statement,
            token,
            typed_name,
        }) = state.approved.get()
        {
            state.approved.set(None);
            let l = limit.get_untracked();
            leptos::task::spawn_local(run(connection, statement, token, typed_name, result, l));
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
        if let Some(rid) = result.get_untracked() {
            leptos::task::spawn_local(async move {
                let _ = ipc::call::<_, ()>("cancel", &serde_json::json!({ "result": rid })).await;
            });
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
                    <button class="btn small" on:click=cancel disabled=move || result.get().is_none()>"Cancel"</button>
                    <button class="btn small primary" on:click=move |_| run_current()
                        disabled=move || live().is_none() title="Ctrl+Enter">"Run"</button>
                </div>
                <Show when=move || state.hints.get()>
                    <div class="hint" role="note">
                        <span>{move || HINTS[hint.get()]}</span>
                        <span class="push">
                            <button class="btn small" on:click=move |_| hint.update(|h| *h = (*h + 1) % HINTS.len())>"Next tip"</button>
                            <button class="btn small" on:click=move |_| {
                                state.hints.set(false);
                                set_setting("hints", serde_json::Value::Bool(false));
                            }>"Don't show tips"</button>
                        </span>
                    </div>
                </Show>
            </div>
            <div class="editor">
                <pre aria-hidden="true">{move || highlighted(dialect(), &text())}</pre>
                <textarea
                    node_ref=area
                    spellcheck="false"
                    aria-label="Statement editor"
                    placeholder="SELECT * FROM …"
                    prop:value=text
                    on:input=move |ev| set_text(event_target_value(&ev))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
                            ev.prevent_default();
                            run_current();
                        }
                    }
                ></textarea>
            </div>
            <Results result=result />
        </main>
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

    #[test]
    fn character_offsets_become_byte_offsets() {
        assert_eq!(byte_offset("héllo", 2), 3);
        assert_eq!(byte_offset("abc", 10), 3);
    }
}
