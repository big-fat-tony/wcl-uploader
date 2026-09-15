mod commands;
mod game;
mod logfile;
mod operation;
mod parser;
mod session;
mod settings;
mod wcl;

use std::sync::Mutex;

use serde::Serialize;
use tauri::{Emitter, Manager};

use commands::AppState;

/// Holds an update found at startup until the user chooses to install it.
pub struct PendingUpdate(pub Mutex<Option<tauri_plugin_updater::Update>>);

#[derive(Clone, Serialize)]
struct UpdateAvailable {
    version: String,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let state = AppState::new().expect("failed to build HTTP client");

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(state)
        .manage(PendingUpdate(Mutex::new(None)))
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = check_for_update(handle).await {
                    log::warn!("update check failed: {e}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_game_versions,
            commands::client_version,
            commands::login,
            commands::auto_login,
            commands::logout,
            commands::get_settings,
            commands::save_settings,
            commands::get_launch_options,
            commands::exit_app,
            commands::detect_log_directory,
            commands::pick_log_file,
            commands::pick_log_directory,
            commands::start_upload,
            commands::start_live_log,
            commands::cancel_operation,
            commands::open_external,
            commands::ui_log,
            commands::parser_selftest,
            commands::install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn check_for_update(app: tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    use tauri_plugin_updater::UpdaterExt;
    let Some(update) = app.updater()?.check().await? else {
        return Ok(());
    };
    log::info!("update available: {} -> {}", update.current_version, update.version);
    let version = update.version.clone();
    app.state::<PendingUpdate>().0.lock().unwrap().replace(update);
    app.emit("update-available", UpdateAvailable { version })?;
    Ok(())
}
