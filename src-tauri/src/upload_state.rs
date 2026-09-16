//! Per-log-file upload progress, so re-uploading a growing combat log resumes
//! the same report and only sends new fights instead of duplicating everything.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Report the file has been (partially) uploaded to.
    pub report_code: String,
    /// Byte offset uploaded through (fights ending at/below this are already sent).
    pub position: u64,
    /// Next segment id to use when appending.
    pub next_segment_id: i64,
    /// Base URL the report lives on (so we only resume onto the same site).
    pub base_url: String,
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    Some(dir.join("upload-state.json"))
}

fn load_all(app: &AppHandle) -> HashMap<String, Entry> {
    path(app)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_all(app: &AppHandle, map: &HashMap<String, Entry>) {
    if let Some(p) = path(app) {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(map) {
            let _ = std::fs::write(p, bytes);
        }
    }
}

/// The file path is the key — WoW names each logging session
/// `WoWCombatLog-<date>_<time>.txt`, so one path == one growing session.
fn key(file_path: &str) -> String {
    file_path.to_string()
}

pub fn get(app: &AppHandle, file_path: &str) -> Option<Entry> {
    load_all(app).remove(&key(file_path))
}

pub fn set(app: &AppHandle, file_path: &str, entry: Entry) {
    let mut map = load_all(app);
    map.insert(key(file_path), entry);
    save_all(app, &map);
}

pub fn clear(app: &AppHandle, file_path: &str) {
    let mut map = load_all(app);
    if map.remove(&key(file_path)).is_some() {
        save_all(app, &map);
    }
}
