//! Bridge to the Warcraft Logs parser page.
//!
//! The parser is a page served by warcraftlogs.com and hosted in a hidden,
//! sandboxed `<iframe>` in the main window. Rust emits `parser-request`
//! events; the frontend relays them to the iframe with `postMessage` and
//! answers through the `parser_response` command. See `docs/PROTOCOL.md` §2.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, Notify};

/// Id embedded in every parser message; the official client uses 1.
pub const IFRAME_ID: u64 = 1;

const LOAD_TIMEOUT: Duration = Duration::from_secs(90);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parser is not loaded")]
    NotReady,
    #[error("parser failed to load: {0}")]
    LoadFailed(String),
    #[error("timed out waiting for the parser ({0})")]
    Timeout(&'static str),
    #[error("parser bridge error: {0}")]
    Bridge(String),
    #[error("parser error: {0}")]
    Parser(String),
    #[error("unexpected parser reply: {0}")]
    Malformed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fight {
    pub event_count: i64,
    pub events_string: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FightsResult {
    #[serde(default)]
    pub fights: Vec<Fight>,
    #[serde(default)]
    pub log_version: Value,
    #[serde(default)]
    pub game_version: Value,
    #[serde(default)]
    pub start_time: Value,
    #[serde(default)]
    pub end_time: Value,
    #[serde(default)]
    pub mythic: Value,
    #[serde(default)]
    pub log_file_details: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterInfo {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub expected_report_code: Option<String>,
    #[serde(default)]
    pub actual_report_code: Option<String>,
    #[serde(default, rename = "lastAssignedActorID")]
    pub last_assigned_actor_id: Value,
    #[serde(default)]
    pub actors_string: String,
    #[serde(default, rename = "lastAssignedAbilityID")]
    pub last_assigned_ability_id: Value,
    #[serde(default)]
    pub abilities_string: String,
    #[serde(default, rename = "lastAssignedTupleID")]
    pub last_assigned_tuple_id: Value,
    #[serde(default)]
    pub tuples_string: String,
    #[serde(default, rename = "lastAssignedPetID")]
    pub last_assigned_pet_id: Value,
    #[serde(default)]
    pub pets_string: String,
    #[serde(default)]
    pub players_string: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogFilePosition {
    pub file_path: String,
    pub current_position: u64,
    pub starting_position: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParseLinesReply {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    parsed_line_count: Value,
    #[serde(default)]
    exception: Value,
    #[serde(default)]
    line: Value,
}

/// Render a JSON scalar the way JavaScript string interpolation would.
pub fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[derive(Default)]
struct LoadState {
    ready: bool,
    error: Option<String>,
}

#[derive(Default)]
pub struct ParserBridge {
    pending: Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_request: AtomicU64,
    load: Mutex<LoadState>,
    load_changed: Notify,
}

impl ParserBridge {
    pub fn is_ready(&self) -> bool {
        self.load.lock().unwrap().ready
    }

    /// Ask the frontend to (re)load the parser iframe and wait for it.
    pub async fn load(&self, app: &AppHandle, url: &str) -> Result<()> {
        {
            let mut state = self.load.lock().unwrap();
            state.ready = false;
            state.error = None;
        }
        self.fail_pending("parser reloaded");
        app.emit("parser-load", json!({ "url": url }))
            .map_err(|e| Error::Bridge(e.to_string()))?;

        let wait = async {
            loop {
                {
                    let state = self.load.lock().unwrap();
                    if state.ready {
                        return Ok(());
                    }
                    if let Some(err) = &state.error {
                        return Err(Error::LoadFailed(err.clone()));
                    }
                }
                self.load_changed.notified().await;
            }
        };
        tokio::time::timeout(LOAD_TIMEOUT, wait)
            .await
            .map_err(|_| Error::Timeout("load"))?
    }

    pub fn mark_loaded(&self) {
        self.load.lock().unwrap().ready = true;
        self.load_changed.notify_waiters();
    }

    pub fn mark_failed(&self, message: String) {
        {
            let mut state = self.load.lock().unwrap();
            state.ready = false;
            state.error = Some(message.clone());
        }
        self.load_changed.notify_waiters();
        self.fail_pending(&message);
    }

    fn fail_pending(&self, reason: &str) {
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(reason.to_string()));
        }
    }

    /// Called by the frontend with the iframe's reply to `request_id`.
    pub fn resolve(&self, request_id: u64, reply: std::result::Result<Value, String>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            let _ = tx.send(reply);
        }
    }

    async fn call(&self, app: &AppHandle, payload: Value, completed: &str) -> Result<Value> {
        if !self.is_ready() {
            return Err(Error::NotReady);
        }
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id, tx);
        app.emit(
            "parser-request",
            json!({ "requestId": request_id, "payload": payload, "completed": completed }),
        )
        .map_err(|e| Error::Bridge(e.to_string()))?;

        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(Error::Bridge(message)),
            Ok(Err(_)) => Err(Error::Bridge("request dropped".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&request_id);
                Err(Error::Timeout("call"))
            }
        }
    }

    fn with_id(mut payload: Value) -> Value {
        payload["id"] = json!(IFRAME_ID);
        payload
    }

    pub async fn get_version(&self, app: &AppHandle) -> Result<Value> {
        let reply = self
            .call(app, json!({ "message": "get-parser-version" }), "get-parser-version-completed")
            .await?;
        Ok(reply.get("data").cloned().unwrap_or(Value::Null))
    }

    pub async fn clear_state(&self, app: &AppHandle) -> Result<()> {
        self.call(app, json!({ "message": "clear-state" }), "clear-state-completed")
            .await
            .map(drop)
    }

    /// WoW logs carry no start date, so this is unused for now; other games
    /// (and the official test operation) send one.
    #[allow(dead_code)]
    pub async fn set_start_date(&self, app: &AppHandle, start_date: &str) -> Result<()> {
        self.call(
            app,
            Self::with_id(json!({ "message": "set-start-date", "startDate": start_date })),
            "set-start-date-completed",
        )
        .await
        .map(drop)
    }

    pub async fn set_live_logging_start_time(&self, app: &AppHandle, start_time_ms: i64) -> Result<()> {
        self.call(
            app,
            Self::with_id(json!({ "message": "set-live-logging-start-time", "startTime": start_time_ms })),
            "set-live-logging-start-time-completed",
        )
        .await
        .map(drop)
    }

    pub async fn set_report_code(&self, app: &AppHandle, code: &str) -> Result<()> {
        self.call(
            app,
            Self::with_id(json!({ "message": "set-report-code", "reportCode": code })),
            "set-report-code-completed",
        )
        .await
        .map(drop)
    }

    pub async fn parse_lines(
        &self,
        app: &AppHandle,
        lines: &[String],
        selected_region: &Value,
        raids_to_upload: &[i64],
        scanning: bool,
        position: &LogFilePosition,
    ) -> Result<()> {
        // The official client sends `regionOrServerId ?? 0`.
        let selected_region = if selected_region.is_null() { json!(0) } else { selected_region.clone() };
        let reply = self
            .call(
                app,
                Self::with_id(json!({
                    "message": "parse-lines",
                    "lines": lines,
                    "selectedRegion": selected_region,
                    "raidsToUpload": raids_to_upload,
                    "scanning": scanning,
                    "logFilePosition": position,
                })),
                "parse-lines-completed",
            )
            .await?;
        let parsed: ParseLinesReply =
            serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))?;
        if parsed.success {
            Ok(())
        } else {
            Err(Error::Parser(format!(
                "Line {} - {} - {}",
                scalar(&parsed.parsed_line_count),
                scalar(&parsed.exception),
                scalar(&parsed.line)
            )))
        }
    }

    pub async fn collect_fights(
        &self,
        app: &AppHandle,
        push_fight_if_needed: bool,
        scanning_only: bool,
    ) -> Result<FightsResult> {
        let reply = self
            .call(
                app,
                Self::with_id(json!({
                    "message": "collect-fights",
                    "pushFightIfNeeded": push_fight_if_needed,
                    "scanningOnly": scanning_only,
                })),
                "collect-fights-completed",
            )
            .await?;
        serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))
    }

    pub async fn collect_in_progress_fight(&self, app: &AppHandle) -> Result<FightsResult> {
        let reply = self
            .call(
                app,
                Self::with_id(json!({ "message": "collect-in-progress-fight" })),
                "collect-in-progress-fight-completed",
            )
            .await?;
        serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))
    }

    pub async fn collect_master_info(&self, app: &AppHandle, report_code: &str) -> Result<MasterInfo> {
        let reply = self
            .call(
                app,
                Self::with_id(json!({ "message": "collect-master-info", "reportCode": report_code })),
                "collect-master-info-completed",
            )
            .await?;
        serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))
    }

    pub async fn clear_fights(&self, app: &AppHandle) -> Result<()> {
        self.call(app, Self::with_id(json!({ "message": "clear-fights" })), "clear-fights-completed")
            .await
            .map(drop)
    }

    /// `clear-fights` + `clear-state`, ignoring errors (used on every exit path).
    pub async fn reset(&self, app: &AppHandle) {
        let _ = self.clear_fights(app).await;
        let _ = self.clear_state(app).await;
    }
}

/// Build the parser page URL for a game version.
pub fn parser_url(base_url: &str, game_version_id: &str) -> String {
    let ts = chrono::Utc::now().timestamp_millis();
    format!(
        "{base_url}/desktop-client/parser?id={IFRAME_ID}&ts={ts}\
         &gameContentDetectionEnabled=false&metersEnabled=false&liveFightDataEnabled=false\
         &gameVersionId={game_version_id}"
    )
}
