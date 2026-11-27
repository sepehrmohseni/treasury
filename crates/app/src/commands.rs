//! The commands the interface calls. Each one hands off to the runtime, the store or the
//! importer and returns plain data; values are formatted for display here, in Rust.

use crate::AppState;
use secrecy::SecretString;
use std::path::PathBuf;
use tauri::State;
use treasury_core::config::{ConnectionConfig, ConnectionId, Engine};
use treasury_core::cursor::ExecutionStatus;
use treasury_core::format::{FormatOptions, display};
use treasury_core::schema::{ChildPage, ChildQuery, ObjectDetails, ObjectPath};
use treasury_core::{Error, Result};
use treasury_ipc::{
    AppFacts, CatalogView, ColumnView, ConnectionView, CopyFormat, CopyView, EditInfo,
    ExportFormat, ExportView, HistoryView, ImportItem, ImportPreview, LiveView, NAME_LIMIT,
    Prepared, RowEdits, RunOutcome, SavedQueryView, SecretInput, TabView, UpdateView, WindowView,
    folder_path,
};
use treasury_store::{RunStatus, SavedQuery, TabState};

#[tauri::command]
pub fn app_facts(state: State<'_, AppState>) -> AppFacts {
    AppFacts {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        secrets_in_memory: state.secrets_in_memory,
    }
}

#[tauri::command]
pub fn connections(state: State<'_, AppState>) -> Result<Vec<ConnectionView>> {
    let rt = &state.runtime;
    rt.store()
        .connections()
        .map_err(|e| Error::internal(e.to_string()))?
        .into_iter()
        .map(|config| {
            let secrets = rt.secrets_of(&config.id).set_slots()?;
            Ok(ConnectionView { config, secrets })
        })
        .collect()
}

/// A new connection's settings with defaults for `engine`.
#[tauri::command]
pub fn new_connection(engine: Engine) -> Result<ConnectionConfig> {
    ConnectionConfig::new("", engine, "localhost")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

#[tauri::command]
pub fn save_connection(
    state: State<'_, AppState>,
    config: ConnectionConfig,
    secrets: Vec<SecretInput>,
) -> Result<()> {
    config.validate()?;
    let rt = &state.runtime;
    rt.store()
        .save_connection(&config, now())
        .map_err(|e| Error::internal(e.to_string()))?;
    let vault = rt.secrets_of(&config.id);
    // A field left empty keeps the secret already saved.
    for s in secrets.into_iter().filter(|s| !s.value.is_empty()) {
        vault.set(s.slot, &SecretString::from(s.value))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_connection(state: State<'_, AppState>, id: ConnectionId) -> Result<()> {
    let rt = &state.runtime;
    rt.disconnect(&id).await;
    rt.secrets_of(&id).clear()?;
    rt.store().close_tabs_of(&id)?;
    rt.store().purge_history(Some(&id))?;
    rt.store()
        .delete_connection(&id)
        .map_err(|e| Error::internal(e.to_string()))
}

async fn live_view(state: &AppState, id: &ConnectionId) -> Result<LiveView> {
    let live = state.runtime.live(id).await?;
    Ok(LiveView {
        info: live.info.clone(),
        capabilities: live.session.capabilities(),
        read_only: live.read_only(),
        unlocked_secs: live.unlocked_until().map(|t| {
            t.saturating_duration_since(std::time::Instant::now())
                .as_secs()
        }),
    })
}

#[tauri::command]
pub async fn connect(state: State<'_, AppState>, id: ConnectionId) -> Result<LiveView> {
    let config = state
        .runtime
        .store()
        .connections()
        .map_err(|e| Error::internal(e.to_string()))?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| Error::config(None, "This connection no longer exists."))?;
    state.runtime.connect(&config).await?;
    live_view(&state, &id).await
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>, id: ConnectionId) -> Result<()> {
    state.runtime.disconnect(&id).await;
    Ok(())
}

#[tauri::command]
pub async fn children(
    state: State<'_, AppState>,
    id: ConnectionId,
    path: ObjectPath,
    query: ChildQuery,
) -> Result<ChildPage> {
    let live = state.runtime.live(&id).await?;
    let tree = live
        .meta
        .introspect()
        .ok_or_else(|| Error::unsupported("This connection has no object tree."))?;
    tree.children(&path, &query).await
}

#[tauri::command]
pub async fn details(
    state: State<'_, AppState>,
    id: ConnectionId,
    path: ObjectPath,
) -> Result<ObjectDetails> {
    let live = state.runtime.live(&id).await?;
    let tree = live
        .meta
        .introspect()
        .ok_or_else(|| Error::unsupported("This connection has no object tree."))?;
    tree.details(&path).await
}

#[tauri::command]
pub async fn ddl(
    state: State<'_, AppState>,
    id: ConnectionId,
    path: ObjectPath,
) -> Result<Option<String>> {
    let live = state.runtime.live(&id).await?;
    let tree = live
        .meta
        .introspect()
        .ok_or_else(|| Error::unsupported("This connection has no object tree."))?;
    tree.ddl(&path).await
}

/// The tables, views and collections of connection `id`, for completion and the palette.
#[tauri::command]
pub async fn catalog(state: State<'_, AppState>, id: ConnectionId) -> Result<CatalogView> {
    Ok(state.runtime.catalog(&id).await?.as_ref().clone())
}

/// The columns of the table or view at `path`, for completion.
#[tauri::command]
pub async fn columns(
    state: State<'_, AppState>,
    id: ConnectionId,
    path: ObjectPath,
) -> Result<Vec<ColumnView>> {
    state.runtime.columns(&id, &path).await
}

#[tauri::command]
pub async fn prepare(
    state: State<'_, AppState>,
    id: ConnectionId,
    statement: String,
) -> Result<Prepared> {
    state.runtime.prepare(&id, &statement).await
}

#[tauri::command]
pub async fn run(
    state: State<'_, AppState>,
    id: ConnectionId,
    statement: String,
    token: u64,
    typed_name: Option<String>,
    limit: bool,
) -> Result<u64> {
    state
        .runtime
        .run(&id, &statement, token, typed_name.as_deref(), limit)
        .await
}

#[tauri::command]
pub async fn window(
    state: State<'_, AppState>,
    result: u64,
    set: usize,
    start: u64,
    count: usize,
) -> Result<WindowView> {
    let w = state
        .runtime
        .window(result, set, start, count.min(2_000))
        .await?;
    let opts = FormatOptions::default();
    Ok(WindowView {
        start: w.start,
        rows: w
            .rows
            .iter()
            .map(|r| r.iter().map(|v| display(v, &opts)).collect())
            .collect(),
    })
}

#[tauri::command]
pub fn status(state: State<'_, AppState>, result: u64) -> Result<ExecutionStatus> {
    state.runtime.status(result)
}

/// Reads statement `result` to its end and returns how it ended.
#[tauri::command]
pub async fn finish(state: State<'_, AppState>, result: u64) -> Result<ExecutionStatus> {
    state.runtime.finish(result).await
}

#[tauri::command]
pub async fn cancel(state: State<'_, AppState>, result: u64) -> Result<()> {
    state.runtime.cancel(result).await
}

#[tauri::command]
pub fn close_result(state: State<'_, AppState>, result: u64) {
    state.runtime.close(result);
}

#[tauri::command]
pub async fn edit_info(state: State<'_, AppState>, result: u64, set: usize) -> Result<EditInfo> {
    state.runtime.edit_info(result, set).await
}

#[tauri::command]
pub async fn edit_review(
    state: State<'_, AppState>,
    result: u64,
    set: usize,
    edits: RowEdits,
) -> Result<Vec<String>> {
    state.runtime.edit_review(result, set, &edits).await
}

#[tauri::command]
pub async fn edit_apply(
    state: State<'_, AppState>,
    result: u64,
    set: usize,
    edits: RowEdits,
) -> Result<u64> {
    state.runtime.edit_apply(result, set, &edits).await
}

/// Rows `start..end` and `columns` of a result set, as text for the clipboard.
#[tauri::command]
pub async fn copy_rows(
    state: State<'_, AppState>,
    result: u64,
    set: usize,
    start: u64,
    end: u64,
    columns: Vec<usize>,
    format: CopyFormat,
) -> Result<CopyView> {
    state
        .runtime
        .copy(result, set, start..end, &columns, format)
        .await
}

#[tauri::command]
pub async fn export_result(
    state: State<'_, AppState>,
    result: u64,
    set: usize,
    path: PathBuf,
    format: ExportFormat,
) -> Result<ExportView> {
    let execution = state.runtime.execution(result)?;
    let format = match format {
        ExportFormat::Csv => treasury_export::Format::csv(),
        ExportFormat::Json => treasury_export::Format::Json,
        ExportFormat::Ndjson => treasury_export::Format::Ndjson,
        ExportFormat::Parquet => treasury_export::Format::Parquet,
    };
    let report =
        treasury_export::export_execution(&execution, set, &path, &format, &|_| {}).await?;
    Ok(ExportView {
        rows: report.rows,
        bytes: report.bytes,
        path: report.path,
    })
}

fn read_import(
    path: &std::path::Path,
) -> Result<(treasury_import::Source, treasury_import::Report)> {
    let source = treasury_import::detect(path).ok_or_else(|| {
        Error::unsupported(
            "This file is not a connection export Treasury can read. It reads DBeaver projects \
             (.dbp, .dbp.zip, data-sources.json), Beekeeper Studio, pgAdmin, DataGrip and \
             MongoDB Compass exports.",
        )
    })?;
    Ok((source, treasury_import::import(path, source)?))
}

#[tauri::command]
pub fn import_preview(path: PathBuf) -> Result<ImportPreview> {
    let (source, report) = read_import(&path)?;
    Ok(ImportPreview {
        source: source.display_name().to_owned(),
        items: report
            .imported
            .iter()
            .enumerate()
            .map(|(index, i)| ImportItem {
                index,
                name: i.config.name.clone(),
                engine: i.config.engine,
                endpoint: serde_json::to_value(&i.config.endpoint)
                    .ok()
                    .map(|v| match v.get("host").zip(v.get("port")) {
                        Some((h, p)) => format!("{}:{}", h.as_str().unwrap_or(""), p),
                        None => v.to_string(),
                    })
                    .unwrap_or_default(),
                database: i.config.database.clone(),
                user: i.config.user.clone(),
                secrets: i.secrets.len(),
                notes: i.notes.clone(),
            })
            .collect(),
        skipped: report
            .skipped
            .into_iter()
            .map(|s| (s.name, s.reason))
            .collect(),
    })
}

/// Saves the chosen connections of an import file, with their secrets, and returns how many.
#[tauri::command]
pub fn import_apply(
    state: State<'_, AppState>,
    path: PathBuf,
    indices: Vec<usize>,
) -> Result<usize> {
    let (_, report) = read_import(&path)?;
    let rt = &state.runtime;
    let mut saved = 0;
    for (i, item) in report.imported.into_iter().enumerate() {
        if !indices.contains(&i) {
            continue;
        }
        rt.store()
            .save_connection(&item.config, now())
            .map_err(|e| Error::internal(e.to_string()))?;
        let vault = rt.secrets_of(&item.config.id);
        for (slot, value) in &item.secrets {
            vault.set(*slot, value)?;
        }
        saved += 1;
    }
    Ok(saved)
}

#[tauri::command]
pub async fn unlock_writes(state: State<'_, AppState>, id: ConnectionId) -> Result<LiveView> {
    state.runtime.unlock_writes(&id).await?;
    live_view(&state, &id).await
}

#[tauri::command]
pub async fn lock_writes(state: State<'_, AppState>, id: ConnectionId) -> Result<LiveView> {
    state.runtime.lock_writes(&id).await?;
    live_view(&state, &id).await
}

/// Settings the interface may read and change; anything else is refused.
const SETTINGS: &[&str] = &[
    "row_limit",
    "theme",
    "density",
    "hints",
    "tutorial_done",
    "history",
];

#[tauri::command]
pub fn setting(state: State<'_, AppState>, key: String) -> Result<Option<serde_json::Value>> {
    if !SETTINGS.contains(&key.as_str()) {
        return Err(Error::unsupported("Unknown setting."));
    }
    state
        .runtime
        .store()
        .setting::<serde_json::Value>(&key)
        .map_err(|e| Error::internal(e.to_string()))
}

#[tauri::command]
pub fn set_setting(
    state: State<'_, AppState>,
    key: String,
    value: serde_json::Value,
) -> Result<()> {
    if !SETTINGS.contains(&key.as_str()) {
        return Err(Error::unsupported("Unknown setting."));
    }
    state
        .runtime
        .store()
        .set_setting(&key, &value)
        .map_err(|e| Error::internal(e.to_string()))
}

/// The latest 200 statements run on connection `id`, newest first, or those matching `search`
/// (every word, the last one as a prefix).
#[tauri::command]
pub fn history(
    state: State<'_, AppState>,
    id: ConnectionId,
    search: Option<String>,
) -> Result<Vec<HistoryView>> {
    let store = state.runtime.store();
    let entries = match search.filter(|s| !s.trim().is_empty()) {
        Some(q) => store.search_history(Some(&id), &q, 200)?,
        None => store.history(Some(&id), None, 200)?,
    };
    Ok(entries
        .into_iter()
        .map(|e| HistoryView {
            id: e.id,
            text: e.text,
            executed_at: e.executed_at,
            duration_ms: e.duration_ms,
            rows: e.rows,
            outcome: match e.status {
                RunStatus::Ok => RunOutcome::Ok,
                RunStatus::Failed => RunOutcome::Failed,
                RunStatus::Cancelled => RunOutcome::Cancelled,
                RunStatus::Unfinished => RunOutcome::Unfinished,
            },
        })
        .collect())
}

/// Deletes the history of connection `id` and returns how many statements it held.
#[tauri::command]
pub fn purge_history(state: State<'_, AppState>, id: ConnectionId) -> Result<u64> {
    Ok(state.runtime.store().purge_history(Some(&id))?)
}

/// The most text a tab or saved query keeps: far more than any script typed or pasted, and a
/// bound on what is sealed and written on every pause in typing.
const TEXT_LIMIT: usize = 16 * 1024 * 1024;

fn check_text(text: &str) -> Result<()> {
    if text.len() > TEXT_LIMIT {
        return Err(Error::config(
            None,
            "This text is over 16 MB, too large to keep in a tab or a saved query.",
        ));
    }
    Ok(())
}

/// `name` trimmed, or an error saying what is wrong with it.
fn checked_name(name: &str, what: &str) -> Result<String> {
    let name = name.trim();
    if name.chars().count() > NAME_LIMIT {
        return Err(Error::config(
            None,
            format!("Keep the {what} under {NAME_LIMIT} characters."),
        ));
    }
    Ok(name.to_owned())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn save_tab_state(state: &AppState, tab: TabView, position: u32) -> Result<()> {
    check_text(&tab.text)?;
    let title = checked_name(&tab.title, "tab title")?;
    state.runtime.store().save_tab(&TabState {
        id: tab.id,
        position: i64::from(position),
        connection: Some(tab.connection),
        title,
        text: tab.text,
        cursor: i64::from(tab.cursor),
        saved_query: tab.saved_query,
    })?;
    Ok(())
}

/// The open tabs, in order, with their positions. Tabs whose text was sealed under a key this
/// session does not have are left out (see the store).
#[tauri::command]
pub fn tabs(state: State<'_, AppState>) -> Result<Vec<(TabView, u32)>> {
    Ok(state
        .runtime
        .store()
        .tabs()?
        .into_iter()
        .filter_map(|t| {
            let position = u32::try_from(t.position).unwrap_or(u32::MAX);
            let tab = TabView {
                id: t.id,
                connection: t.connection?,
                title: t.title,
                text: t.text,
                cursor: u32::try_from(t.cursor).unwrap_or(0),
                saved_query: t.saved_query,
            };
            Some((tab, position))
        })
        .collect())
}

/// Opens a tab at `position` and returns it with its new identifier.
#[tauri::command]
pub fn open_tab(
    state: State<'_, AppState>,
    connection: ConnectionId,
    title: String,
    text: String,
    saved_query: Option<String>,
    position: u32,
) -> Result<TabView> {
    let tab = TabView {
        id: ConnectionId::random()?.0,
        connection,
        title,
        text,
        cursor: 0,
        saved_query,
    };
    save_tab_state(&state, tab.clone(), position)?;
    Ok(tab)
}

/// Keeps a tab's text, title, cursor and place in the strip.
#[tauri::command]
pub fn save_tab(state: State<'_, AppState>, tab: TabView, position: u32) -> Result<()> {
    save_tab_state(&state, tab, position)
}

#[tauri::command]
pub fn close_tab(state: State<'_, AppState>, id: String) -> Result<()> {
    Ok(state.runtime.store().close_tab(&id)?)
}

/// Saved queries, by folder and name.
#[tauri::command]
pub fn saved_queries(state: State<'_, AppState>) -> Result<Vec<SavedQueryView>> {
    Ok(state
        .runtime
        .store()
        .saved_queries()?
        .into_iter()
        .map(|q| SavedQueryView {
            id: q.id,
            folder: q.folder,
            name: q.name,
            connection: q.connection,
        })
        .collect())
}

fn gone() -> Error {
    Error::config(None, "This saved query no longer exists.")
}

#[tauri::command]
pub fn saved_query_text(state: State<'_, AppState>, id: String) -> Result<String> {
    state
        .runtime
        .store()
        .saved_query_text(&id)?
        .ok_or_else(gone)
}

/// Saves a query: a new one when `id` is `None`. Without `text`, the saved text stays, so a
/// rename or a move to another folder changes nothing else.
#[tauri::command]
pub fn save_query(
    state: State<'_, AppState>,
    id: Option<String>,
    name: String,
    folder: String,
    connection: Option<ConnectionId>,
    text: Option<String>,
) -> Result<SavedQueryView> {
    let name = checked_name(&name, "name")?;
    if name.is_empty() {
        return Err(Error::config(None, "Give the query a name."));
    }
    let folder = checked_name(&folder_path(&folder), "folder")?;
    let store = state.runtime.store();
    let id = match id {
        Some(id) => id,
        None => ConnectionId::random()?.0,
    };
    let text = match text {
        Some(t) => t,
        None => store.saved_query_text(&id)?.ok_or_else(gone)?,
    };
    check_text(&text)?;
    let query = SavedQuery {
        id,
        folder,
        name,
        connection,
        updated_at: now_ms(),
    };
    store.save_query(&query, &text)?;
    Ok(SavedQueryView {
        id: query.id,
        folder: query.folder,
        name: query.name,
        connection: query.connection,
    })
}

#[tauri::command]
pub fn delete_saved_query(state: State<'_, AppState>, id: String) -> Result<()> {
    Ok(state.runtime.store().delete_saved_query(&id)?)
}

#[tauri::command]
pub fn host_key_answer(state: State<'_, AppState>, question: u64, accept: bool) {
    state.prompt.answer(question, accept);
}

fn update_error(e: tauri_plugin_updater::Error) -> Error {
    Error::new(
        treasury_core::ErrorKind::Protocol,
        format!("Treasury could not update itself: {e}"),
    )
}

/// The updater, pointed at the release feed. Debug builds take `TREASURY_UPDATE_URL` instead,
/// so the desktop test can serve its own feed; packages are still checked against the public
/// key built into the app.
fn updater(app: &tauri::AppHandle) -> Result<tauri_plugin_updater::Updater> {
    use tauri_plugin_updater::UpdaterExt;
    #[allow(unused_mut, reason = "only debug builds change the endpoint")]
    let mut builder = app.updater_builder();
    #[cfg(debug_assertions)]
    if let Some(url) = std::env::var("TREASURY_UPDATE_URL")
        .ok()
        .and_then(|u| u.parse().ok())
    {
        builder = builder.endpoints(vec![url]).map_err(update_error)?;
    }
    builder.build().map_err(update_error)
}

/// A newer release, if there is one.
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> Result<Option<UpdateView>> {
    let update = updater(&app)?.check().await.map_err(update_error)?;
    Ok(update.map(|u| UpdateView {
        version: u.version.clone(),
        notes: u.body.clone(),
    }))
}

/// Downloads the newer release, checks its signature, installs it and restarts.
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<()> {
    let Some(update) = updater(&app)?.check().await.map_err(update_error)? else {
        return Ok(());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(update_error)?;
    app.restart()
}
