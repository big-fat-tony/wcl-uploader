//! Persisted user settings. The password never touches the JSON file: it goes
//! to the OS credential store (Windows Credential Manager) via `keyring`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

const KEYRING_SERVICE: &str = "dev.nymann.logs-uploader";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub email: String,
    pub remember_password: bool,
    pub game_version_id: String,
    /// Last-used report options as sent by the UI (guildId, regionOrServerId, ...).
    pub report: Option<Value>,
    pub live_directory: String,
    pub include_entire_file: Option<bool>,
    pub real_time: Option<bool>,
}

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let p = path(app)?;
    let json = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(p, json).map_err(|e| e.to_string())
}

pub fn store_password(email: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, email)
        .and_then(|e| e.set_password(password))
        .map_err(|e| format!("credential store: {e}"))
}

pub fn load_password(email: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, email)
        .and_then(|e| e.get_password())
        .ok()
}

pub fn delete_password(email: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, email) {
        let _ = entry.delete_credential();
    }
}
