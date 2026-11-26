//! The Treasury desktop app: a Tauri window around the runtime. Commands here translate between
//! the interface and the runtime and hold no logic of their own.

#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod prompt;

use std::sync::Arc;
use tauri::Manager;
use treasury_core::driver::Driver;
use treasury_runtime::Runtime;
use treasury_store::Store;
use treasury_vault::{MemoryStore, Sealer, SecretStore};

/// What every command can reach.
pub struct AppState {
    /// Connections, statements and results.
    pub runtime: Runtime,
    /// Passwords are only kept in memory because no system keychain was found.
    pub secrets_in_memory: bool,
    /// Asks the window about SSH host keys seen for the first time.
    pub prompt: Arc<prompt::GuiPrompt>,
}

fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(treasury_driver_postgres::PostgresDriver),
        Arc::new(treasury_driver_mysql::MySqlDriver::mysql()),
        Arc::new(treasury_driver_mysql::MySqlDriver::mariadb()),
        Arc::new(treasury_driver_mssql::MsSqlDriver),
        Arc::new(treasury_driver_mongo::MongoDriver),
    ]
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let data = app.path().app_data_dir()?;
    let cache = app.path().app_cache_dir()?;
    std::fs::create_dir_all(&data)?;
    let spill = cache.join("spill");
    std::fs::create_dir_all(&spill)?;
    let (secrets, in_memory): (Arc<dyn SecretStore>, bool) =
        match treasury_vault::Keychain::platform() {
            Ok(k) => (Arc::new(k), false),
            Err(e) => {
                tracing::warn!("no system keychain, keeping secrets in memory: {e}");
                (Arc::new(MemoryStore::default()), true)
            }
        };
    let key = treasury_store::field_key(secrets.as_ref())?;
    let store = Arc::new(Store::open(&data.join("treasury.db"), Sealer::new(key))?);
    let prompt = Arc::new(prompt::GuiPrompt::new(app.handle().clone()));
    let runtime = Runtime::new(
        store,
        secrets,
        drivers(),
        treasury_tunnel::KnownHosts::new(data.join("known_hosts")),
        prompt.clone(),
        spill,
    );
    app.manage(AppState {
        runtime,
        secrets_in_memory: in_memory,
        prompt,
    });
    Ok(())
}

#[allow(
    clippy::print_stderr,
    reason = "a window that cannot open has nowhere else to say why"
)]
fn main() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(setup)
        .invoke_handler(tauri::generate_handler![
            commands::app_facts,
            commands::connections,
            commands::new_connection,
            commands::save_connection,
            commands::delete_connection,
            commands::connect,
            commands::disconnect,
            commands::children,
            commands::details,
            commands::ddl,
            commands::prepare,
            commands::run,
            commands::window,
            commands::status,
            commands::cancel,
            commands::close_result,
            commands::edit_info,
            commands::edit_review,
            commands::edit_apply,
            commands::export_result,
            commands::import_preview,
            commands::import_apply,
            commands::unlock_writes,
            commands::lock_writes,
            commands::setting,
            commands::set_setting,
            commands::history,
            commands::host_key_answer,
            commands::check_update,
            commands::install_update,
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("Treasury could not start: {e}");
        std::process::exit(1);
    }
}
