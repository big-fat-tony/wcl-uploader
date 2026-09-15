mod commands;
mod game;
mod logfile;
mod operation;
mod parser;
mod session;
mod wcl;

use commands::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let state = AppState::new().expect("failed to build HTTP client");

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::list_game_versions,
            commands::client_version,
            commands::login,
            commands::logout,
            commands::detect_log_directory,
            commands::pick_log_file,
            commands::pick_log_directory,
            commands::start_upload,
            commands::start_live_log,
            commands::cancel_operation,
            commands::parser_loaded,
            commands::parser_failed,
            commands::parser_response,
            commands::open_external,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
