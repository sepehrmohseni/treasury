//! The workspace for the open connection: toolbar, hints, the SQL editor and its results.
//!
//! The editor is a transparent textarea over highlighted text; the highlighting comes from
//! Treasury's own SQL tokenizer, so it reads a statement the way the guardrails do.

use crate::complete;
use crate::grid::Results;
use crate::home::set_setting;
use crate::ipc;
use crate::state::{Approved, Dialog, ui};
use crate::tabs;
use leptos::prelude::*;
use treasury_core::analyze::{Analyzer, StatementClass};
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine, Environment};
use treasury_core::driver::SqlDialect;
use treasury_core::exec::ExecutionStatus;
use treasury_core::schema::{ObjectKind, ObjectNode};
use treasury_ipc::{LiveView, Prepared};
use treasury_sql::highlight::{SpanKind, highlight};
use treasury_sql::narrow::Sort;
use treasury_sql::{SqlAnalyzer, render};
use wasm_bindgen::JsCast;

/// Short tips, one shown at a time.
const HINTS: &[&str] = &[
    "Ctrl+Enter runs the statement under the cursor; select text to run just that.",
    "Plain SELECTs get a 1,000-row limit. Untick \"Limit\" in the toolbar to read everything.",
    "Production connections open read-only. \"Unlock writes\" lasts 15 minutes.",
    "Right-click a connection in the navigator to edit it or open a SQL editor on it.",
    "UPDATE and DELETE on shared databases show how many rows they change before they run.",
    "Export streams straight to the file, so results larger than memory export fine.",
    "Double-click a cell of a one-table result to edit it. Nothing is saved until you review the statements.",
    "Double-click a table in the navigator to open it: Properties lists its columns, Data shows its rows.",
    "On a table's Data page, type a condition such as id > 10 in the filter box, or click a column header to sort.",
    "F7 shows the selected row beside the grid, one field per line.",
    "Every tab keeps its text as you type, so it is still there after a restart.",
    "Ctrl+S saves the tab as a query; saved queries are under Queries on the left.",
    "History on the left lists what ran on this connection; search it, or click one to open it.",
    "Shift+Ctrl+Enter runs every statement in the tab, one after another, each with its own result.",
    "Table and column names are suggested as you type; Ctrl+Space shows them without typing.",
    "Ctrl+K finds any connection, table, saved query or action by part of its name.",
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

/// How a connection's editor text splits into statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// SQL, split by the dialect's analyzer.
    Sql(SqlDialect),
    /// One MongoDB command, which may span lines.
    Json,
    /// Redis: one command per line; a line starting with `#` is a comment.
    Lines,
}

impl Lang {
    pub fn of(engine: Engine) -> Self {
        match (dialect_of(engine), engine.family()) {
            (Some(d), _) => Lang::Sql(d),
            (None, treasury_core::config::Family::KeyValue) => Lang::Lines,
            (None, _) => Lang::Json,
        }
    }

    pub fn dialect(self) -> Option<SqlDialect> {
        match self {
            Lang::Sql(d) => Some(d),
            _ => None,
        }
    }
}

/// The byte ranges of the command lines of `text`: not empty, not comments.
fn command_lines(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let lead = line.len() - line.trim_start().len();
            out.push(at + lead..at + lead + trimmed.len());
        }
        at += line.len();
    }
    out
}

/// A word of a Redis command, quoted when it must be, as redis-cli reads it.
fn redis_word(word: &str) -> String {
    let bare = !word.is_empty()
        && word
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\'));
    if bare {
        return word.to_owned();
    }
    format!("{:?}", word)
}

/// The statement that shows the rows of a table, view or collection in the tree, or the value
/// of a Redis key, or `None` for other objects.
pub fn browse_statement(config: &ConnectionConfig, node: &ObjectNode) -> Option<String> {
    if node.kind == ObjectKind::Key {
        let key = redis_word(&node.path.leaf()?.name);
        let read = match node.detail.as_deref() {
            Some("hash") => format!("HGETALL {key}"),
            Some("list") => format!("LRANGE {key} 0 -1"),
            Some("set") => format!("SMEMBERS {key}"),
            Some("zset") => format!("ZRANGE {key} 0 -1 WITHSCORES"),
            Some("stream") => format!("XRANGE {key} - + COUNT 1000"),
            Some("json") => format!("JSON.GET {key} $"),
            _ => format!("GET {key}"),
        };
        let own = config
            .database
            .as_deref()
            .and_then(|d| d.trim_start_matches("db").parse::<i64>().ok())
            .unwrap_or(0);
        // A key in another database is read after selecting it.
        return Some(
            match node
                .path
                .find(ObjectKind::Database)
                .and_then(|d| d.parse::<i64>().ok())
            {
                Some(db) if db != own => format!("SELECT {db}\n{read}"),
                _ => read,
            },
        );
    }
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
            mongo_find(name, db, "{}", &[])
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

/// The find command for collection `name` of database `db`: the documents matching `filter`, a
/// JSON document, sorted by `order`.
fn mongo_find(name: &str, db: &str, filter: &str, order: &[Sort]) -> String {
    // The command name comes first, so the text is written out rather than serialized.
    let mut text = format!(
        "{{\"find\": {}, \"filter\": {filter}",
        serde_json::Value::from(name)
    );
    if !order.is_empty() {
        let keys: Vec<String> = order
            .iter()
            .map(|s| {
                let way = if s.descending { -1 } else { 1 };
                format!("{}: {way}", serde_json::Value::from(s.column.as_str()))
            })
            .collect();
        text.push_str(&format!(", \"sort\": {{{}}}", keys.join(", ")));
    }
    text.push_str(&format!(", \"$db\": {}}}", serde_json::Value::from(db)));
    text
}

/// The statement a table's Data page runs: its rows that match `filter`, sorted by `order`.
/// The filter is a SQL condition, or a JSON query document for MongoDB. `Ok(None)` for objects
/// without rows; an error says why the filter cannot be used.
pub fn data_statement(
    config: &ConnectionConfig,
    node: &ObjectNode,
    filter: &str,
    order: &[Sort],
) -> Result<Option<String>, String> {
    let Some(base) = browse_statement(config, node) else {
        return Ok(None);
    };
    let filter = filter.trim();
    if filter.is_empty() && order.is_empty() {
        return Ok(Some(base));
    }
    match (config.engine, dialect_of(config.engine)) {
        (Engine::MongoDb, _) => {
            let filter = if filter.is_empty() { "{}" } else { filter };
            if !matches!(
                serde_json::from_str::<serde_json::Value>(filter),
                Ok(serde_json::Value::Object(_))
            ) {
                return Err(
                    "The filter must be a JSON query document, such as {\"status\": \"open\"}."
                        .into(),
                );
            }
            let (Some(name), database) = (
                node.path.0.last().map(|p| p.name.as_str()),
                node.path.find(ObjectKind::Database),
            ) else {
                return Ok(None);
            };
            let db = database.or(config.database.as_deref()).unwrap_or("test");
            Ok(Some(mongo_find(name, db, filter, order)))
        }
        (_, Some(d)) => treasury_sql::narrow::narrow(d, &base, filter, order).map(Some),
        (_, None) => Err("Rows of this object cannot be filtered or sorted.".into()),
    }
}

/// Every statement of `text`, in order: SQL by its dialect, Redis by line, and a MongoDB command
/// as one.
pub fn statements_of(lang: Lang, text: &str) -> Vec<String> {
    let ranges = match lang {
        Lang::Json => return vec![text.trim().to_owned()],
        Lang::Lines => command_lines(text),
        Lang::Sql(d) => SqlAnalyzer::new(d)
            .classify(text)
            .iter()
            .map(|s| s.range.clone())
            .collect(),
    };
    ranges
        .into_iter()
        .filter_map(|r| text.get(r))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The statement to run: the selection, or the statement under the cursor (for Redis, the
/// command on the cursor's line, or the last one above it).
pub fn statement_at(lang: Lang, text: &str, start: usize, end: usize) -> String {
    if end > start {
        return text.get(start..end).unwrap_or("").trim().to_owned();
    }
    let ranges = match lang {
        Lang::Json => return text.trim().to_owned(),
        Lang::Lines => command_lines(text),
        Lang::Sql(d) => SqlAnalyzer::new(d)
            .classify(text)
            .iter()
            .map(|s| s.range.clone())
            .collect(),
    };
    let found = match lang {
        Lang::Lines => ranges.iter().rev().find(|r| r.start <= start),
        _ => ranges
            .iter()
            .find(|r| r.start <= start && start <= r.end + 1),
    };
    found
        .or(ranges.last())
        .and_then(|r| text.get(r.clone()))
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
pub fn highlighted(dialect: Option<SqlDialect>, text: &str) -> AnyView {
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

/// Where a run's result goes among the tab's results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Instead of all of them: a statement run on its own.
    Replace,
    /// After them: the next statement of a script.
    Append,
    /// Instead of this one: its statement run again after its rows were saved.
    Instead(u64),
}

/// Starts `statement` in tab `tab`: asks the guardrails first, then runs it or opens their
/// dialog. `step` is its place in a script.
fn begin(tab: String, statement: String, limit: bool, slot: Slot, step: Option<(usize, usize)>) {
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
            Err(e) => failed(&tab, Some(e.to_string())),
            Ok(p) if p.refused.is_none() && p.count.is_none() && p.name_required.is_none() => {
                if p.class == StatementClass::Ddl {
                    // Its tables may change; completion reads them again.
                    crate::complete::fetch::forget(&connection);
                }
                run(
                    tab, connection, statement, p.token, None, limit, slot, p.class,
                )
                .await;
            }
            Ok(p) => state.dialog.set(Dialog::Confirm {
                tab,
                connection,
                statement,
                prepared: p,
                step,
            }),
        }
    });
}

/// Starts `statement` on its own in tab `tab`.
pub fn start(tab: String, statement: String, limit: bool) {
    begin(tab, statement, limit, Slot::Replace, None);
}

/// Runs every statement of a script in tab `tab`, one after another; each goes through the
/// guardrails and is read to its end before the next one starts. It stops at the first that
/// fails or is not confirmed.
pub fn run_all(tab: String, statements: Vec<String>, limit: bool) {
    let state = ui();
    let statements: Vec<String> = statements.into_iter().filter(|s| !s.is_empty()).collect();
    if statements.len() < 2 {
        if let Some(one) = statements.into_iter().next() {
            start(tab, one, limit);
        }
        return;
    }
    if let Some(out) = state.outputs.with_untracked(|m| m.get(&tab).copied()) {
        close_all(out);
    }
    state.scripts.update(|m| {
        m.insert(
            tab.clone(),
            crate::state::Script {
                statements,
                next: 0,
                limit,
            },
        );
    });
    step(tab);
}

/// Starts the next statement of the script in tab `tab`, or ends the script.
fn step(tab: String) {
    let state = ui();
    let Some(script) = state.scripts.with_untracked(|m| m.get(&tab).cloned()) else {
        return;
    };
    let total = script.statements.len();
    let Some(statement) = script.statements.get(script.next).cloned() else {
        state.scripts.update(|m| {
            m.remove(&tab);
        });
        state
            .notice
            .set(Some(format!("Ran all {total} statements.")));
        return;
    };
    state.scripts.update(|m| {
        if let Some(s) = m.get_mut(&tab) {
            s.next += 1;
        }
    });
    let at = (script.next + 1, total);
    begin(tab, statement, script.limit, Slot::Append, Some(at));
}

/// Says that the statement a script in tab `tab` is at failed, with `error` when it has no result
/// to show it in, and stops the script. Without a script, `error` is said alone.
fn failed(tab: &str, error: Option<String>) {
    let state = ui();
    let script = state.scripts.with_untracked(|m| m.get(tab).cloned());
    let text = match (script, error) {
        (Some(s), e) => {
            let head = format!(
                "Statement {} of {} failed, so the ones after it did not run",
                s.next,
                s.statements.len()
            );
            match e {
                Some(e) => format!("{head}: {e}"),
                None => format!("{head}."),
            }
        }
        (None, Some(e)) => e,
        (None, None) => return,
    };
    state.notice.set(Some(text));
    stop_script(tab);
}

/// Stops the script in tab `tab`, if one runs: the statements after the current one do not run.
pub fn stop_script(tab: &str) {
    ui().scripts.update(|m| {
        m.remove(tab);
    });
}

/// Closes every result of `out`.
fn close_all(out: crate::state::Output) {
    let state = ui();
    let old = out.results.get_untracked();
    if old.is_empty() {
        return;
    }
    out.results.set(Vec::new());
    leptos::task::spawn_local(async move {
        for rid in old {
            let _ = ipc::call::<_, ()>("close_result", &serde_json::json!({ "result": rid })).await;
        }
        // A result still being read is recorded in the history when it closes.
        state.history_changed();
    });
}

/// Runs a statement the guardrails allowed, puts its result in `slot`, and goes on with the
/// script when it is part of one. Once a statement of class `class` that changes objects has
/// ended, the navigator reads the connection's objects again.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    tab: String,
    connection: ConnectionId,
    statement: String,
    token: u64,
    typed_name: Option<String>,
    limit: bool,
    slot: Slot,
    class: StatementClass,
) {
    let state = ui();
    let Some(out) = state.outputs.with_untracked(|m| m.get(&tab).copied()) else {
        return;
    };
    match slot {
        Slot::Replace => close_all(out),
        Slot::Append => {}
        Slot::Instead(old) => {
            let _ = ipc::call::<_, ()>("close_result", &serde_json::json!({ "result": old })).await;
            state.history_changed();
        }
    }
    let reply = ipc::call::<_, u64>(
        "run",
        &serde_json::json!({
            "id": connection, "statement": statement, "token": token,
            "typedName": typed_name, "limit": limit,
        }),
    )
    .await;
    let rid = match reply {
        Ok(rid) => rid,
        Err(e) => {
            failed(&tab, Some(e.to_string()));
            return;
        }
    };
    out.statements.update(|m| {
        m.insert(rid, statement);
    });
    out.results.update(|r| match slot {
        Slot::Instead(old) => match r.iter_mut().find(|x| **x == old) {
            Some(x) => *x = rid,
            None => r.push(rid),
        },
        Slot::Append => r.push(rid),
        Slot::Replace => *r = vec![rid],
    });
    let ddl = class == StatementClass::Ddl;
    if slot != Slot::Append {
        if ddl {
            leptos::task::spawn_local(async move {
                if let Ok(status) =
                    ipc::call::<_, ExecutionStatus>("finish", &serde_json::json!({ "result": rid }))
                        .await
                    && status.error.is_none()
                {
                    crate::navigator::refresh(&connection);
                }
            });
        }
        return;
    }
    // The next statement of a script starts only once this one has ended.
    match ipc::call::<_, ExecutionStatus>("finish", &serde_json::json!({ "result": rid })).await {
        Ok(status) => {
            let broke = status.error.is_some();
            if ddl && !broke {
                crate::navigator::refresh(&connection);
            }
            out.statuses.update(|m| {
                m.insert(rid, status);
            });
            state.history_changed();
            if broke {
                // Its result shows the error.
                failed(&tab, None);
            } else {
                step(tab);
            }
        }
        Err(e) => failed(&tab, Some(e.to_string())),
    }
}

/// The editor's text laid out for reading: SQL through the formatter, MongoDB commands as
/// indented JSON. `None` when it cannot be laid out without changing it.
fn formatted(lang: Lang, text: &str) -> Option<String> {
    match lang {
        Lang::Sql(d) => treasury_sql::format::format(d, text),
        Lang::Json => reindent_json(text),
        Lang::Lines => None,
    }
}

/// `text` as indented JSON with every key where it was (a command's name must come first), or
/// `None` if it is not one JSON value.
fn reindent_json(text: &str) -> Option<String> {
    let original: serde_json::Value = serde_json::from_str(text).ok()?;
    let chars: Vec<char> = text.trim().chars().collect();
    let mut out = String::new();
    let mut depth = 0usize;
    let (mut in_string, mut escaped) = (false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        i += 1;
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                let close = if c == '{' { '}' } else { ']' };
                let next = chars[i..].iter().position(|n| !n.is_whitespace());
                if next.is_some_and(|n| chars[i + n] == close) {
                    out.push(c);
                    out.push(close);
                    i += next.unwrap_or(0) + 1;
                } else {
                    out.push(c);
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    (serde_json::from_str::<serde_json::Value>(&out).ok()? == original).then_some(out)
}

/// Replaces `start..end` (in the DOM's units) of the editor with `text` the way typing would, so
/// Ctrl+Z takes it back. `false` when the webview refused.
fn replace_in_editor(el: &web_sys::HtmlTextAreaElement, start: u32, end: u32, text: &str) -> bool {
    use wasm_bindgen::JsCast;
    let _ = el.focus();
    let _ = el.set_selection_range(start, end);
    document()
        .dyn_into::<web_sys::HtmlDocument>()
        .ok()
        .and_then(|d| {
            d.exec_command_with_show_ui_and_value("insertText", false, text)
                .ok()
        })
        .unwrap_or(false)
}

/// The suggestions shown: where the word starts and the caret is (bytes), and where the list
/// goes (pixels in the editor).
#[derive(Clone, Debug, PartialEq)]
struct Popup {
    start: usize,
    caret: usize,
    items: Vec<complete::Suggestion>,
    selected: usize,
    x: f64,
    y: f64,
}

thread_local! {
    /// A suggestion is being written into the editor, so its input event opens no new list.
    static ACCEPTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Where the caret is in the editor, in pixels: the probe, laid out like the editor's text,
/// holds the text before the caret and a mark where it ends.
fn caret_point(probe: &web_sys::HtmlDivElement, before: &str) -> (f64, f64) {
    use wasm_bindgen::JsCast;
    probe.set_text_content(Some(before));
    let Ok(mark) = document().create_element("span") else {
        return (0.0, 0.0);
    };
    mark.set_text_content(Some("\u{200b}"));
    let _ = probe.append_child(&mark);
    let point = mark
        .dyn_ref::<web_sys::HtmlElement>()
        .map_or((0.0, 0.0), |m| {
            (f64::from(m.offset_left()), f64::from(m.offset_top()))
        });
    probe.set_text_content(None);
    point
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
    let lang = move || config().map(|c| Lang::of(c.engine)).unwrap_or(Lang::Json);
    let dialect = move || lang().dialect();
    let current = move || state.current();
    let text = move || {
        current()
            .and_then(|t| state.tab_text.with(|m| m.get(&t).cloned()))
            .unwrap_or_default()
    };
    let output = move || current().and_then(|t| state.outputs.with(|m| m.get(&t).copied()));
    // The shown tab is an object editor rather than a SQL editor.
    let object_mode = Memo::new(move |_| state.current_object().is_some());
    let data_page = move || {
        current().is_some_and(|t| {
            state
                .pages
                .with(|m| m.get(&t) == Some(&crate::state::ObjectPage::Data))
        })
    };

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
            lang(),
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
        let statement_for_tab = statement.clone();
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
        // A Redis key in another database is read after a SELECT: two commands, run in turn.
        let parts = statements_of(lang(), &statement);
        let go = move |id: String| {
            if parts.len() > 1 {
                run_all(id, parts, l);
            } else {
                start(id, statement.clone(), l);
            }
        };
        match existing {
            Some(id) => {
                tabs::show(id.clone());
                go(id);
            }
            None => {
                let text = statement_for_tab.clone();
                leptos::task::spawn_local(async move {
                    if let Some(id) = tabs::open(connection, title, text, None).await {
                        go(id);
                    }
                });
            }
        }
    });

    // A statement the guardrail dialog approved, on its own or as the next of a script.
    Effect::new(move |_| {
        if let Some(Approved {
            tab,
            connection,
            statement,
            token,
            typed_name,
            class,
        }) = state.approved.get()
        {
            state.approved.set(None);
            state.notice.set(None);
            let script = state
                .scripts
                .with_untracked(|m| m.get(&tab).map(|s| s.limit));
            let (l, slot) = match script {
                Some(l) => (l, Slot::Append),
                None => (limit.get_untracked(), Slot::Replace),
            };
            leptos::task::spawn_local(run(
                tab, connection, statement, token, typed_name, l, slot, class,
            ));
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

    // Cancels the statement running in the tab, and what a script would run after it.
    let cancel = move |_| {
        if let Some(id) = state.current_untracked() {
            stop_script(&id);
        }
        if let Some(rid) = output().and_then(|o| o.last()) {
            leptos::task::spawn_local(async move {
                let _ = ipc::call::<_, ()>("cancel", &serde_json::json!({ "result": rid })).await;
            });
        }
    };

    // Runs every statement of the tab, or of the selection, one after another.
    let run_everything = move || {
        let (Some(id), Some(el)) = (state.current(), area.get_untracked()) else {
            return;
        };
        let value = el.value();
        let s = el.selection_start().ok().flatten().unwrap_or(0);
        let e = el.selection_end().ok().flatten().unwrap_or(s);
        let text = if e > s {
            value
                .get(byte_offset(&value, s)..byte_offset(&value, e))
                .unwrap_or("")
                .to_owned()
        } else {
            value
        };
        state.notice.set(None);
        run_all(id, statements_of(lang(), &text), limit.get_untracked());
    };

    // Formats the selection, or all of the text when nothing is selected.
    let format_text = move || {
        let (Some(id), Some(el)) = (state.current(), area.get_untracked()) else {
            return;
        };
        let value = el.value();
        let s = el.selection_start().ok().flatten().unwrap_or(0);
        let e = el.selection_end().ok().flatten().unwrap_or(s);
        let (s, e) = if e > s {
            (s, e)
        } else {
            (0, value.encode_utf16().count() as u32)
        };
        let (from, to) = (byte_offset(&value, s), byte_offset(&value, e));
        let Some(piece) = value.get(from..to) else {
            return;
        };
        if lang() == Lang::Lines {
            state.notice.set(Some(
                "Redis commands are one per line, so there is nothing to lay out.".into(),
            ));
            return;
        }
        match formatted(lang(), piece) {
            Some(f) if f == piece => {}
            Some(f) => {
                if !replace_in_editor(&el, s, e, &f) {
                    tabs::set_text(&id, format!("{}{f}{}", &value[..from], &value[to..]));
                }
            }
            None => state.notice.set(Some(
                "Treasury could not lay this out without changing it, so it is left as it is."
                    .into(),
            )),
        }
    };

    // The caret moved: remember it for when the tab is shown again.
    let track_caret = move || {
        if let (Some(id), Some(el)) = (state.current(), area.get_untracked()) {
            tabs::set_cursor(&id, caret(&el));
        }
    };

    // Completion: the suggestions for the word being typed, under the caret.
    let popup = RwSignal::new(None::<Popup>);
    let probe = NodeRef::<leptos::html::Div>::new();
    // A word is being typed, so names arriving from the app refresh the list.
    let completing = RwSignal::new(false);
    let update_completion = move || {
        let (Some(connection), Some(d), Some(el)) = (
            state.active.get_untracked(),
            state
                .active_config_untracked()
                .and_then(|c| dialect_of(c.engine)),
            area.get_untracked(),
        ) else {
            popup.set(None);
            return;
        };
        let value = el.value();
        let at = byte_offset(&value, caret(&el));
        let Some(ctx) = complete::context(d, &value, at) else {
            popup.set(None);
            return;
        };
        complete::fetch::catalog(&connection);
        let catalog = state
            .catalogs
            .with_untracked(|m| m.get(&connection).map(|c| c.entries.clone()))
            .unwrap_or_default();
        let statement = statement_at(Lang::Sql(d), &value, at, at);
        for t in complete::tables_named(d, &statement, &catalog) {
            complete::fetch::columns(&connection, t);
        }
        let items = state.table_columns.with_untracked(|cols| {
            complete::suggest(d, &ctx, &statement, &catalog, &|e| {
                cols.get(&(connection.clone(), e.path.clone())).cloned()
            })
        });
        let Some(probe) = probe.get_untracked().filter(|_| !items.is_empty()) else {
            popup.set(None);
            return;
        };
        let (x, y) = caret_point(&probe, value.get(..at).unwrap_or(""));
        popup.set(Some(Popup {
            start: ctx.start,
            caret: at,
            items,
            selected: 0,
            x,
            y,
        }));
    };
    // What the command palette asks the editor to do.
    Effect::new(move |_| {
        let Some(command) = state.command.get() else {
            return;
        };
        state.command.set(None);
        request_animation_frame(move || match command {
            crate::state::EditorCommand::RunAll => run_everything(),
            crate::state::EditorCommand::Format => format_text(),
        });
    });

    // Names arriving from the app refresh the list while a word is being typed.
    Effect::new(move |_| {
        state.catalogs.track();
        state.table_columns.track();
        if completing.get_untracked() {
            update_completion();
        }
    });
    let close_completion = move || {
        completing.set(false);
        popup.set(None);
    };
    let accept = move |i: usize| {
        let (Some(p), Some(el)) = (popup.get_untracked(), area.get_untracked()) else {
            return;
        };
        let Some(s) = p.items.get(i) else {
            return;
        };
        close_completion();
        let value = el.value();
        let units = |b: usize| {
            u32::try_from(value.get(..b).unwrap_or("").encode_utf16().count()).unwrap_or(0)
        };
        ACCEPTING.with(|a| a.set(true));
        let done = replace_in_editor(&el, units(p.start), units(p.caret), &s.insert);
        ACCEPTING.with(|a| a.set(false));
        if !done && let Some(id) = state.current_untracked() {
            let mut text = value.clone();
            text.replace_range(p.start..p.caret, &s.insert);
            tabs::set_text(&id, text);
        }
    };

    view! {
        <main
            class="workspace"
            class:object-mode=move || object_mode.get()
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
                    <label class="detail" hidden=move || object_mode.get()><input type="checkbox" prop:checked=move || limit.get()
                        on:change=move |ev| limit.set(event_target_checked(&ev)) />" Limit"</label>
                    {move || live().filter(|_| config().is_some_and(|c| c.read_only || c.environment == Environment::Production)).map(|l| view! {
                        <button class="btn small" on:click=toggle_writes>
                            {if l.read_only { "Unlock writes" } else { "Lock writes" }}
                        </button>
                    })}
                    <button class="btn small format" hidden=move || object_mode.get() on:click=move |_| format_text()
                        disabled=move || current().is_none() title="Shift+Alt+F">"Format"</button>
                    <button class="btn small save-query" hidden=move || object_mode.get() on:click=move |_| tabs::save_current()
                        disabled=move || current().is_none() title="Ctrl+S">"Save query"</button>
                    <button class="btn small" on:click=cancel
                        disabled=move || output().is_none_or(|o| o.results.with(Vec::is_empty))>"Cancel"</button>
                    <button class="btn small run-all" hidden=move || object_mode.get() on:click=move |_| run_everything()
                        disabled=move || live().is_none()
                        title=move || if live().is_some() { "Shift+Ctrl+Enter" } else { "Connect first" }>"Run all"</button>
                    <button class="btn small primary" hidden=move || object_mode.get() on:click=move |_| run_current()
                        disabled=move || live().is_none()
                        title=move || if live().is_some() { "Ctrl+Enter" } else { "Connect first" }>"Run"</button>
                </div>
                <ConnectionBanner />
                <Show when=move || state.hints.get()>
                    <div class="hint" role="note">
                        <span>{move || HINTS[hint.get()]}</span>
                        <span class="push">
                            <button class="btn small" on:click=move |_| hint.update(|h| *h = (*h + 1) % HINTS.len())>"Next tip"</button>
                            <button class="btn small no-tips" on:click=move |_| {
                                state.hints.set(false);
                                set_setting(treasury_ipc::Setting::Hints, serde_json::Value::Bool(false));
                            }>"Don't show tips"</button>
                        </span>
                    </div>
                </Show>
                <TabStrip />
            </div>
            <div class="editor" hidden=move || object_mode.get()>
                <pre aria-hidden="true">{move || highlighted(dialect(), &text())}</pre>
                <div class="caret-probe" node_ref=probe aria-hidden="true"></div>
                {move || popup.get().map(|p| view! {
                    <ul class="completions" role="listbox" aria-label="Suggestions"
                        style:left=format!("{}px", p.x) style:top=format!("{}px", p.y + 20.0)>
                        {p.items.into_iter().enumerate().map(|(i, s)| view! {
                            <li role="option" class:on=i == p.selected aria-selected=(i == p.selected).to_string()
                                on:mousedown=move |ev| { ev.prevent_default(); accept(i); }>
                                <span class="c-label">{s.label}</span>
                                <span class="c-detail">{s.detail}</span>
                            </li>
                        }).collect_view()}
                    </ul>
                })}
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
                        // A letter or a dot (re)opens the suggestions; deleting refines them.
                        let typed = ev.dyn_ref::<web_sys::InputEvent>().and_then(web_sys::InputEvent::data);
                        let wordy = typed.as_deref().and_then(|t| t.chars().last()).is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                        let deleting = ev.dyn_ref::<web_sys::InputEvent>().is_some_and(|e| e.input_type().starts_with("delete"));
                        if ACCEPTING.with(std::cell::Cell::get) {
                        } else if wordy || (deleting && completing.get_untracked()) {
                            completing.set(true);
                            update_completion();
                        } else {
                            close_completion();
                        }
                    }
                    on:keyup=move |_| track_caret()
                    on:click=move |_| { track_caret(); close_completion(); }
                    on:blur=move |_| {
                        close_completion();
                        if let Some(id) = state.current() { tabs::keep(&id) }
                    }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        let ctrl = ev.ctrl_key() || ev.meta_key();
                        // The open list takes the arrows, Enter, Tab and Escape.
                        if let Some(p) = popup.get_untracked().filter(|_| !ctrl) {
                            let n = p.items.len();
                            match ev.key().as_str() {
                                "ArrowDown" | "ArrowUp" => {
                                    ev.prevent_default();
                                    let next = if ev.key() == "ArrowDown" { (p.selected + 1) % n } else { (p.selected + n - 1) % n };
                                    popup.update(|p| if let Some(p) = p { p.selected = next; });
                                    request_animation_frame(|| {
                                        if let Some(el) = document().query_selector(".completions li.on").ok().flatten() {
                                            el.scroll_into_view_with_bool(false);
                                        }
                                    });
                                    return;
                                }
                                "Enter" | "Tab" => {
                                    ev.prevent_default();
                                    accept(p.selected);
                                    return;
                                }
                                "Escape" => {
                                    ev.prevent_default();
                                    close_completion();
                                    return;
                                }
                                _ => {}
                            }
                        }
                        if ctrl && ev.key() == " " {
                            // Ctrl+Space asks for suggestions without typing.
                            ev.prevent_default();
                            completing.set(true);
                            update_completion();
                        } else if ctrl && ev.shift_key() && ev.key() == "Enter" {
                            ev.prevent_default();
                            run_everything();
                        } else if ctrl && ev.key() == "Enter" {
                            ev.prevent_default();
                            run_current();
                        } else if ctrl && ev.key().eq_ignore_ascii_case("s") {
                            ev.prevent_default();
                            tabs::save_current();
                        } else if ev.shift_key() && ev.alt_key() && ev.code() == "KeyF" {
                            ev.prevent_default();
                            format_text();
                        }
                    }
                ></textarea>
            </div>
            <crate::objects::ObjectHeader />
            <div class="panes" hidden=move || object_mode.get() && !data_page()>
                // Every tab keeps its result, and its unsaved grid edits, while another is shown.
                <For
                    each=move || state.tabs.get()
                    key=|t| t.id.clone()
                    children=move |t| {
                        let id = t.id.clone();
                        let shown = id.clone();
                        let out = state.outputs.with_untracked(|m| m.get(&id).copied()).unwrap_or_else(crate::state::Output::new);
                        // Saved grid edits: run the statement again to show the rows as they are now.
                        // A table's Data page filters and sorts on the server.
                        let rows = t.object.as_ref().is_some_and(|o| crate::navigator::has_rows(o.kind));
                        let sorting = rows.then(|| {
                            let tab = id.clone();
                            crate::grid::Sorting {
                                order: crate::objects::order_of(id.clone()),
                                on_sort: Callback::new(move |(column, add): (String, bool)| {
                                    crate::objects::sort_by(&tab, &column, add)
                                }),
                            }
                        });
                        let filter_tab = id.clone();
                        let rerun = Callback::new(move |rid: u64| {
                            if let Some(statement) = out.statements.with_untracked(|m| m.get(&rid).cloned()) {
                                begin(id.clone(), statement, limit.get_untracked(), Slot::Instead(rid), None);
                            }
                        });
                        view! {
                            <div class="pane" class:with-filter=rows hidden=move || current().as_ref() != Some(&shown)>
                                {rows.then(|| view! { <crate::objects::DataFilter tab=filter_tab.clone() /> })}
                                <Results out=out on_saved=rerun sorting=sorting />
                            </div>
                        }
                    }
                />
            </div>
            <crate::objects::ObjectProps />
        </main>
    }
}

/// Under the toolbar while the active connection is not open: that it is opening, or why it
/// could not be opened and what to do about it.
#[component]
fn ConnectionBanner() -> impl IntoView {
    let state = ui();
    move || {
        let id = state.active.get()?;
        if state.live.with(|m| m.contains_key(&id)) {
            return None;
        }
        let config = state.active_config()?;
        let name = config.name.clone();
        let retry = id.clone();
        if state.connecting.with(|c| c.contains(&id)) {
            let through = config
                .ssh
                .as_ref()
                .map(|s| format!(" through {}", s.target.host))
                .unwrap_or_default();
            return Some(
                view! {
                    <div class="conn-banner" role="status">{format!("Connecting to {name}{through}…")}</div>
                }
                .into_any(),
            );
        }
        let edit = move |_| {
            state
                .dialog
                .set(Dialog::Connection(Box::new(config.clone())))
        };
        Some(match state.failed.with(|m| m.get(&id).cloned()) {
            Some(error) => view! {
                <div class="conn-banner failed" role="alert">
                    <div class="what">
                        <strong>{format!("Could not connect to {name}.")}</strong>
                        <div class="why">{error}</div>
                    </div>
                    <span class="push">
                        <button class="btn small" on:click=edit>"Edit connection"</button>
                        <button class="btn small primary retry" on:click=move |_| crate::sidebar::connect(retry.clone())>"Try again"</button>
                    </span>
                </div>
            }
            .into_any(),
            None => view! {
                <div class="conn-banner" role="status">
                    <span>{format!("{name} is not connected.")}</span>
                    <span class="push">
                        <button class="btn small primary" on:click=move |_| crate::sidebar::connect(retry.clone())>"Connect"</button>
                    </span>
                </div>
            }
            .into_any(),
        })
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
                    let object = state.tab(&id).is_some_and(|t| t.object.is_some());
                    view! {
                        <div class="etab" class:object=object class:on=move || state.current().as_ref() == Some(&on)>
                            <button class="etab-title" role="tab"
                                aria-selected=move || (state.current().as_ref() == Some(&id)).to_string()
                                title=hover
                                on:click=move |_| tabs::show(show.clone())>
                                {object.then(|| view! { <span class="etab-kind" aria-hidden="true">"▦"</span> })}
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
            statement_at(Lang::Sql(SqlDialect::Postgres), text, second, second),
            "SELECT 2"
        );
        assert_eq!(
            statement_at(
                Lang::Sql(SqlDialect::Postgres),
                text,
                text.len(),
                text.len()
            ),
            "SELECT 3"
        );
        assert_eq!(
            statement_at(Lang::Sql(SqlDialect::Postgres), text, 0, 8),
            "SELECT 1"
        );
        assert_eq!(
            statement_at(Lang::Json, " {\"ping\": 1} ", 0, 0),
            "{\"ping\": 1}"
        );
        // Redis: the line of the cursor, or the last command above it.
        let redis = "SET a 1\n# note\nGET a\n\n";
        assert_eq!(statement_at(Lang::Lines, redis, 2, 2), "SET a 1");
        assert_eq!(
            statement_at(Lang::Lines, redis, redis.len(), redis.len()),
            "GET a"
        );
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
    fn a_data_page_filters_and_sorts_on_the_server() {
        use ObjectKind::{Collection, Database, Group, Schema, Table};
        let by = |column: &str, descending: bool| Sort {
            column: column.to_owned(),
            descending,
        };
        let (c, n) = node(
            Engine::Postgres,
            Some("shop"),
            &[(Schema, "shop"), (Group, "Tables"), (Table, "orders")],
        );
        assert_eq!(
            data_statement(&c, &n, "", &[]).unwrap().unwrap(),
            browse_statement(&c, &n).unwrap()
        );
        assert_eq!(
            data_statement(&c, &n, "total > 100", &[by("id", true)])
                .unwrap()
                .unwrap(),
            "SELECT * FROM \"shop\".\"orders\"\nWHERE total > 100\nORDER BY \"id\" DESC"
        );
        assert!(data_statement(&c, &n, "1 = 1; drop table orders", &[]).is_err());

        let (c, n) = node(
            Engine::MongoDb,
            Some("app"),
            &[(Database, "app"), (Collection, "users")],
        );
        assert_eq!(
            data_statement(
                &c,
                &n,
                r#"{"age": {"$gt": 30}}"#,
                &[by("name", false), by("age", true)]
            )
            .unwrap()
            .unwrap(),
            r#"{"find": "users", "filter": {"age": {"$gt": 30}}, "sort": {"name": 1, "age": -1}, "$db": "app"}"#
        );
        assert_eq!(
            data_statement(&c, &n, "", &[by("_id", true)])
                .unwrap()
                .unwrap(),
            r#"{"find": "users", "filter": {}, "sort": {"_id": -1}, "$db": "app"}"#
        );
        for bad in [
            "age > 30",
            "[1]",
            r#"{"a": 1} {"b": 2}"#,
            r#"{"a": 1}, "$db": "admin""#,
        ] {
            assert!(data_statement(&c, &n, bad, &[]).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_script_splits_into_its_statements() {
        assert_eq!(
            statements_of(
                Lang::Sql(SqlDialect::Postgres),
                "create table t (a int);\n\ninsert into t values (1); select * from t;"
            ),
            [
                "create table t (a int)",
                "insert into t values (1)",
                "select * from t"
            ]
        );
        assert_eq!(
            statements_of(Lang::Json, " {\"ping\": 1} "),
            ["{\"ping\": 1}"]
        );
        assert_eq!(
            statements_of(Lang::Lines, "# setup\nSET a 1\n\n  GET a\n"),
            ["SET a 1", "GET a"]
        );
    }

    #[test]
    fn json_is_indented_with_its_keys_in_order() {
        let f = reindent_json(
            r#"{"find": "users", "filter": {"name": "a,b {x}", "tags": []}, "$db": "app"}"#,
        )
        .unwrap();
        assert_eq!(
            f,
            "{\n  \"find\": \"users\",\n  \"filter\": {\n    \"name\": \"a,b {x}\",\n    \"tags\": []\n  },\n  \"$db\": \"app\"\n}"
        );
        assert!(f.find("find").unwrap() < f.find("filter").unwrap());
        assert_eq!(
            reindent_json(r#"{"a": "say \"hi\""}"#).unwrap(),
            "{\n  \"a\": \"say \\\"hi\\\"\"\n}"
        );
        assert!(reindent_json("db.users.find({})").is_none());
    }

    #[test]
    fn redis_keys_open_with_the_command_for_their_type() {
        let mut config = ConnectionConfig::new("c", Engine::Redis, "h").unwrap();
        let key = |db: &str, name: &str, kind: &str| ObjectNode {
            path: treasury_core::schema::ObjectPath::root()
                .child(ObjectKind::Database, db)
                .child(ObjectKind::Key, name),
            kind: ObjectKind::Key,
            name: name.into(),
            expandable: false,
            detail: Some(kind.into()),
        };
        assert_eq!(
            browse_statement(&config, &key("0", "user:1", "hash")).unwrap(),
            "HGETALL user:1"
        );
        assert_eq!(
            browse_statement(&config, &key("0", "a b", "string")).unwrap(),
            "GET \"a b\""
        );
        assert_eq!(
            browse_statement(&config, &key("3", "q", "list")).unwrap(),
            "SELECT 3\nLRANGE q 0 -1"
        );
        config.database = Some("3".into());
        assert_eq!(
            browse_statement(&config, &key("3", "q", "zset")).unwrap(),
            "ZRANGE q 0 -1 WITHSCORES"
        );
    }

    #[test]
    fn character_offsets_become_byte_offsets() {
        assert_eq!(byte_offset("héllo", 2), 3);
        assert_eq!(byte_offset("abc", 10), 3);
    }
}
