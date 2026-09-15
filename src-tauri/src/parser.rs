//! Runs the Warcraft Logs parser out-of-browser.
//!
//! The parser JavaScript the site serves is executed inside a Node process
//! (`resources/parser-harness.js`) that this module spawns and drives over
//! stdin/stdout — one JSON command per line, one JSON reply per line. This
//! avoids hosting the parser in a WebView2 iframe (which the site gates behind
//! Cloudflare and a browser session). See `docs/PROTOCOL.md` §2 for the
//! command set; the harness maps each command onto the site's own parser
//! globals.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parser is not running")]
    NotReady,
    #[error("could not find Node.js. Install Node 18+ or set LU_NODE to the node executable.")]
    NodeNotFound,
    #[error("parser failed to start: {0}")]
    StartFailed(String),
    #[error("timed out waiting for the parser ({0})")]
    Timeout(&'static str),
    #[error("parser I/O error: {0}")]
    Io(String),
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
    error: Value,
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

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Default)]
pub struct ParserBridge {
    proc: Mutex<Option<Process>>,
    version: Mutex<Value>,
}

impl ParserBridge {
    pub async fn is_ready(&self) -> bool {
        self.proc.lock().await.is_some()
    }

    /// Spawn the Node harness and load the parser code. Replaces any running
    /// instance. Returns the parser version.
    pub async fn start(
        &self,
        app: &AppHandle,
        gamedata_code: &str,
        parser_code: &str,
        parser_version: Value,
    ) -> Result<Value> {
        self.shutdown().await;
        *self.version.lock().await = parser_version;

        let node = find_node(app).ok_or(Error::NodeNotFound)?;
        let harness = harness_path(app)?;
        let mut child = tokio::process::Command::new(&node)
            .arg(&harness)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::StartFailed(format!("spawning {node:?}: {e}")))?;

        let mut stdin = child.stdin.take().ok_or_else(|| Error::StartFailed("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::StartFailed("no stdout".into()))?;
        let mut stdout = BufReader::new(stdout);

        let payload = json!({ "gamedataCode": gamedata_code, "parserCode": parser_code });
        write_line(&mut stdin, &payload).await?;

        let ready: Value = tokio::time::timeout(STARTUP_TIMEOUT, read_line(&mut stdout))
            .await
            .map_err(|_| Error::Timeout("startup"))??;
        if ready.get("ready").and_then(Value::as_bool) != Some(true) {
            let msg = ready.get("error").and_then(Value::as_str).unwrap_or("unknown");
            return Err(Error::StartFailed(msg.to_string()));
        }
        let version = ready.get("parserVersion").cloned().unwrap_or(Value::Null);
        *self.proc.lock().await = Some(Process { child, stdin, stdout });
        Ok(version)
    }

    pub async fn shutdown(&self) {
        if let Some(mut proc) = self.proc.lock().await.take() {
            let _ = proc.stdin.shutdown().await;
            let _ = proc.child.kill().await;
        }
    }

    async fn call(&self, command: Value) -> Result<Value> {
        let mut guard = self.proc.lock().await;
        let proc = guard.as_mut().ok_or(Error::NotReady)?;
        write_line(&mut proc.stdin, &command).await?;
        tokio::time::timeout(CALL_TIMEOUT, read_line(&mut proc.stdout))
            .await
            .map_err(|_| Error::Timeout("call"))?
    }

    pub async fn get_version(&self, _app: &AppHandle) -> Result<Value> {
        let stored = self.version.lock().await.clone();
        if !stored.is_null() {
            return Ok(stored);
        }
        let reply = self.call(json!({ "action": "get-parser-version" })).await?;
        Ok(reply.get("parserVersion").cloned().unwrap_or(Value::Null))
    }

    pub async fn clear_state(&self, _app: &AppHandle) -> Result<()> {
        self.call(json!({ "action": "clear-state" })).await.map(drop)
    }

    #[allow(dead_code)]
    pub async fn set_start_date(&self, _app: &AppHandle, start_date: &str) -> Result<()> {
        self.call(json!({ "action": "set-start-date", "startDate": start_date })).await.map(drop)
    }

    pub async fn set_live_logging_start_time(&self, _app: &AppHandle, start_time_ms: i64) -> Result<()> {
        self.call(json!({ "action": "set-live-logging-start-time", "startTime": start_time_ms }))
            .await
            .map(drop)
    }

    pub async fn set_report_code(&self, _app: &AppHandle, code: &str) -> Result<()> {
        self.call(json!({ "action": "set-report-code", "reportCode": code })).await.map(drop)
    }

    pub async fn parse_lines(
        &self,
        _app: &AppHandle,
        lines: &[String],
        selected_region: &Value,
        raids_to_upload: &[i64],
        scanning: bool,
        position: &LogFilePosition,
    ) -> Result<()> {
        let selected_region = if selected_region.is_null() { json!(0) } else { selected_region.clone() };
        let reply = self
            .call(json!({
                "action": "parse-lines",
                "lines": lines,
                "selectedRegion": selected_region,
                "raidsToUpload": raids_to_upload,
                "scanning": scanning,
                "logFilePosition": position,
            }))
            .await?;
        let parsed: ParseLinesReply =
            serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))?;
        if parsed.success {
            Ok(())
        } else {
            Err(Error::Parser(format!(
                "line {}: {} ({})",
                scalar(&parsed.parsed_line_count),
                scalar(&parsed.error),
                scalar(&parsed.line)
            )))
        }
    }

    pub async fn collect_fights(
        &self,
        _app: &AppHandle,
        push_fight_if_needed: bool,
        scanning_only: bool,
    ) -> Result<FightsResult> {
        let reply = self
            .call(json!({
                "action": "collect-fights",
                "pushFightIfNeeded": push_fight_if_needed,
                "scanningOnly": scanning_only,
            }))
            .await?;
        serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))
    }

    pub async fn collect_in_progress_fight(&self, _app: &AppHandle) -> Result<FightsResult> {
        let reply = self.call(json!({ "action": "collect-in-progress-fight" })).await?;
        serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))
    }

    pub async fn collect_master_info(&self, _app: &AppHandle, report_code: &str) -> Result<MasterInfo> {
        let reply = self
            .call(json!({ "action": "collect-master-info", "reportCode": report_code }))
            .await?;
        serde_json::from_value(reply).map_err(|e| Error::Malformed(e.to_string()))
    }

    pub async fn clear_fights(&self, _app: &AppHandle) -> Result<()> {
        self.call(json!({ "action": "clear-fights" })).await.map(drop)
    }

    /// `clear-fights` + `clear-state`, ignoring errors (used on exit paths).
    pub async fn reset(&self, app: &AppHandle) {
        let _ = self.clear_fights(app).await;
        let _ = self.clear_state(app).await;
    }
}

async fn write_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value).map_err(|e| Error::Io(e.to_string()))?;
    line.push(b'\n');
    stdin.write_all(&line).await.map_err(|e| Error::Io(e.to_string()))?;
    stdin.flush().await.map_err(|e| Error::Io(e.to_string()))
}

async fn read_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let n = stdout.read_line(&mut line).await.map_err(|e| Error::Io(e.to_string()))?;
    if n == 0 {
        return Err(Error::Io("parser exited".into()));
    }
    serde_json::from_str(line.trim_end()).map_err(|e| Error::Malformed(format!("{e}: {line}")))
}

/// Locate `parser-harness.js` next to the app (bundled resource) or in the dev
/// source tree.
fn harness_path(app: &AppHandle) -> Result<PathBuf> {
    if let Some(p) = resolve_resource(app, "parser-harness.js") {
        return Ok(p);
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/parser-harness.js");
    if dev.exists() {
        return Ok(dev);
    }
    Err(Error::StartFailed("parser-harness.js not found".into()))
}

/// Resolve a bundled resource. Resources are declared with a `resources/`
/// prefix, so Tauri places them under `<resourceDir>/resources/`; also try the
/// resource root for robustness across bundlers.
fn resolve_resource(app: &AppHandle, name: &str) -> Option<PathBuf> {
    use tauri::path::BaseDirectory::Resource;
    for candidate in [format!("resources/{name}"), name.to_string()] {
        if let Ok(p) = app.path().resolve(&candidate, Resource) {
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Find a Node.js executable: `LU_NODE`, then the bundled runtime shipped next
/// to the app, then `node` on `PATH`.
fn find_node(app: &AppHandle) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("LU_NODE") {
        let p = PathBuf::from(explicit);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    if let Some(bundled) = resolve_resource(app, exe) {
        return Some(bundled);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(exe)).find(|c| c.exists())
}

/// The parser page URL to fetch the parser code from.
pub fn parser_url(base_url: &str, game_version_id: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let ts = chrono::Utc::now().timestamp_millis();
    format!(
        "{base_url}/desktop-client/parser?id=1&ts={ts}\
         &gameContentDetectionEnabled=false&metersEnabled=false&liveFightDataEnabled=false\
         &gameVersionId={game_version_id}"
    )
}
