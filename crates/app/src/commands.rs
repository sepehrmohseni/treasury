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
    AppFacts, ConnectionView, ExportFormat, ExportView, ImportItem, ImportPreview, LiveView,
    Prepared, SecretInput, WindowView,
};
use treasury_store::HistoryEntry;

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

#[tauri::command]
pub async fn cancel(state: State<'_, AppState>, result: u64) -> Result<()> {
    state.runtime.cancel(result).await
}

#[tauri::command]
pub fn close_result(state: State<'_, AppState>, result: u64) {
    state.runtime.close(result);
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

#[tauri::command]
pub fn history(
    state: State<'_, AppState>,
    id: Option<ConnectionId>,
    search: Option<String>,
) -> Result<Vec<HistoryEntry>> {
    let store = state.runtime.store();
    match search.filter(|s| !s.trim().is_empty()) {
        Some(q) => store.search_history(id.as_ref(), &q, 200),
        None => store.history(id.as_ref(), None, 200),
    }
    .map_err(|e| Error::internal(e.to_string()))
}

#[tauri::command]
pub fn host_key_answer(state: State<'_, AppState>, question: u64, accept: bool) {
    state.prompt.answer(question, accept);
}
