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
        Arc::new(treasury_driver_redis::RedisDriver::redis()),
        Arc::new(treasury_driver_redis::RedisDriver::valkey()),
    ]
}

/// Where the main window goes, in physical pixels: centred at its size when it fits the screen's
/// work area (the screen less its taskbar or dock), otherwise made 90% of that area, centred, and
/// maximised, so restoring it gives a window that fits.
#[derive(Debug, PartialEq, Eq)]
struct Placement {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximize: bool,
}

/// [`Placement`] for a window whose outer size is `size` on a work area at `origin` of `area`.
fn placement(origin: (i32, i32), area: (u32, u32), size: (u32, u32)) -> Placement {
    let fits = size.0 <= area.0 && size.1 <= area.1;
    let (width, height) = if fits {
        size
    } else {
        (
            size.0.min(area.0.saturating_mul(9) / 10),
            size.1.min(area.1.saturating_mul(9) / 10),
        )
    };
    let centre = |start: i32, room: u32, used: u32| {
        start.saturating_add(i32::try_from(room.saturating_sub(used) / 2).unwrap_or(0))
    };
    Placement {
        x: centre(origin.0, area.0, width),
        y: centre(origin.1, area.1, height),
        width,
        height,
        maximize: !fits,
    }
}

/// Fits the main window to the screen it opens on, then shows it. A window taller than the screen
/// less its taskbar hides its own bottom edge, and the status bar and dialog buttons with it.
fn fit_window(window: &tauri::WebviewWindow) {
    let fit = || -> tauri::Result<()> {
        let Some(monitor) = window.current_monitor()?.or(window.primary_monitor()?) else {
            return Ok(());
        };
        let area = monitor.work_area();
        let outer = window.outer_size()?;
        let inner = window.inner_size()?;
        let p = placement(
            (area.position.x, area.position.y),
            (area.size.width, area.size.height),
            (outer.width, outer.height),
        );
        if p.maximize {
            // The size set is the inside of the window; the frame comes on top.
            window.set_size(tauri::PhysicalSize::new(
                p.width
                    .saturating_sub(outer.width.saturating_sub(inner.width)),
                p.height
                    .saturating_sub(outer.height.saturating_sub(inner.height)),
            ))?;
        }
        window.set_position(tauri::PhysicalPosition::new(p.x, p.y))?;
        if p.maximize {
            window.maximize()?;
        }
        Ok(())
    };
    if let Err(e) = fit() {
        tracing::warn!("could not fit the window to the screen: {e}");
    }
    if let Err(e) = window.show() {
        tracing::warn!("could not show the window: {e}");
    }
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // The window is made hidden, so it is only seen once it fits the screen.
    if let Some(window) = app.get_webview_window("main") {
        fit_window(&window);
    }
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
            commands::catalog,
            commands::columns,
            commands::prepare,
            commands::run,
            commands::window,
            commands::status,
            commands::finish,
            commands::cancel,
            commands::close_result,
            commands::edit_info,
            commands::edit_review,
            commands::edit_apply,
            commands::copy_rows,
            commands::export_result,
            commands::import_preview,
            commands::import_apply,
            commands::unlock_writes,
            commands::lock_writes,
            commands::setting,
            commands::set_setting,
            commands::history,
            commands::purge_history,
            commands::tabs,
            commands::open_tab,
            commands::save_tab,
            commands::close_tab,
            commands::saved_queries,
            commands::saved_query_text,
            commands::save_query,
            commands::delete_saved_query,
            commands::host_key_answer,
            commands::check_update,
            commands::install_update,
        ])
        .build(tauri::generate_context!());
    match result {
        // On the way out, results still open complete their history entries.
        Ok(app) => app.run(|app, event| {
            if let tauri::RunEvent::Exit = event
                && let Some(state) = app.try_state::<AppState>()
            {
                state.runtime.close_all();
            }
        }),
        Err(e) => {
            eprintln!("Treasury could not start: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Placement, placement};

    #[test]
    fn a_window_that_fits_is_centred_at_its_size() {
        assert_eq!(
            placement((0, 0), (1920, 1032), (1280, 830)),
            Placement {
                x: 320,
                y: 101,
                width: 1280,
                height: 830,
                maximize: false
            }
        );
        // On a second screen, to the right of the first.
        assert_eq!(
            placement((1920, 0), (2560, 1400), (1600, 1000)),
            Placement {
                x: 2400,
                y: 200,
                width: 1600,
                height: 1000,
                maximize: false
            }
        );
    }

    #[test]
    fn a_window_taller_than_the_screen_is_made_to_fit_and_maximised() {
        // 1280 x 800 at 125% with its title bar, on 1920 x 1080 less a 48-pixel taskbar.
        assert_eq!(
            placement((0, 0), (1920, 1032), (1600, 1039)),
            Placement {
                x: 160,
                y: 52,
                width: 1600,
                height: 928,
                maximize: true
            }
        );
        // A small screen, with the taskbar at the top.
        assert_eq!(
            placement((0, 40), (1366, 728), (1280, 830)),
            Placement {
                x: 68,
                y: 76,
                width: 1229,
                height: 655,
                maximize: true
            }
        );
    }
}
