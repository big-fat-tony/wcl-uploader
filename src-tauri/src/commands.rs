//! Tauri commands exposed to the frontend.
//!
//! Every command is `async` so it runs off the main thread; the webview cookie
//! API dispatches to the main thread and must not be called from it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use url::Url;

use crate::game::{self, GameVersion};
use crate::operation::{self, Ctx, LiveParams, UploadParams};
use crate::parser::ParserBridge;
use crate::session::{self, Credentials, Session};
use crate::wcl::{self, UserInfo};

pub struct AppState {
    pub wcl: Arc<wcl::Client>,
    pub parser: Arc<ParserBridge>,
    pub session: Mutex<Option<Session>>,
    pub credentials: Mutex<Option<Credentials>>,
    /// Cancellation flag of the running operation, if any.
    pub operation: Mutex<Option<Arc<AtomicBool>>>,
}

impl AppState {
    pub fn new() -> Result<Self, wcl::Error> {
        Ok(Self {
            wcl: Arc::new(wcl::Client::new()?),
            parser: Arc::new(ParserBridge::default()),
            session: Mutex::new(None),
            credentials: Mutex::new(None),
            operation: Mutex::new(None),
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResult {
    pub game_version_id: String,
    pub base_url: String,
    pub user: UserInfo,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub ok: bool,
    pub cancelled: bool,
    pub report_code: Option<String>,
    pub report_url: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn list_game_versions() -> Vec<GameVersion> {
    game::GAME_VERSIONS.to_vec()
}

#[tauri::command]
pub async fn client_version() -> String {
    format!("{} (protocol {})", env!("CARGO_PKG_VERSION"), wcl::CLIENT_VERSION)
}

#[tauri::command]
pub async fn login(
    app: AppHandle,
    state: State<'_, AppState>,
    email: String,
    password: String,
    game_version_id: String,
) -> Result<LoginResult, String> {
    let gv = game::find(&game_version_id).ok_or("unknown game version")?;
    let base_url = Url::parse(gv.base_url).map_err(|e| e.to_string())?;
    let user = state
        .wcl
        .login(&base_url, &email, &password, &game_version_id)
        .await
        .map_err(|e| e.to_string())?;

    *state.credentials.lock().unwrap() = Some(Credentials { email, password });
    *state.session.lock().unwrap() = Some(Session {
        base_url: base_url.clone(),
        game_version_id: game_version_id.clone(),
    });

    session::sync_cookies(&app, &state.wcl, &base_url)?;
    let url = crate::parser::parser_url(gv.base_url, &game_version_id);
    // Load the parser now so the first upload does not have to wait for it.
    if let Err(e) = state.parser.load(&app, &url).await {
        let _ = app.emit("app-log", serde_json::json!({ "message": format!("Parser load failed: {e}") }));
    }

    Ok(LoginResult { game_version_id, base_url: gv.base_url.to_string(), user })
}

#[tauri::command]
pub async fn logout(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let session = state.session.lock().unwrap().take();
    *state.credentials.lock().unwrap() = None;
    if let Some(session) = session {
        let _ = state.wcl.logout(&session.base_url).await;
        session::clear_cookies(&app, &session.base_url);
    }
    let _ = app.emit("parser-load", serde_json::json!({ "url": "about:blank" }));
    Ok(())
}

#[tauri::command]
pub async fn detect_log_directory(game_version_id: String) -> Option<String> {
    let gv = game::find(&game_version_id)?;
    game::detect_log_directory(gv.install_dir).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn pick_log_file(start_dir: Option<String>) -> Option<String> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Choose a combat log")
        .add_filter("Combat logs", &["txt"]);
    if let Some(dir) = start_dir.filter(|d| std::path::Path::new(d).is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file().await.map(|f| f.path().to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn pick_log_directory(start_dir: Option<String>) -> Option<String> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose the WoW Logs directory");
    if let Some(dir) = start_dir.filter(|d| std::path::Path::new(d).is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder().await.map(|f| f.path().to_string_lossy().into_owned())
}

fn begin_operation(app: &AppHandle, state: &AppState, kind: &str) -> Result<Ctx, String> {
    let session = state.session.lock().unwrap().clone().ok_or("not logged in")?;
    let credentials = state.credentials.lock().unwrap().clone().ok_or("not logged in")?;
    let mut slot = state.operation.lock().unwrap();
    if slot.as_ref().map(|c| !c.load(Ordering::Relaxed)).unwrap_or(false) {
        return Err("an operation is already running".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *slot = Some(cancel.clone());
    Ok(Ctx::new(
        app.clone(),
        state.wcl.clone(),
        state.parser.clone(),
        session.base_url,
        session.game_version_id,
        credentials,
        cancel,
        kind,
    ))
}

fn finish_operation(app: &AppHandle, state: &AppState, ctx: &Ctx, result: operation::Result<String>) -> OperationResult {
    *state.operation.lock().unwrap() = None;
    let cancelled = ctx.cancel.load(Ordering::Relaxed);
    let out = match result {
        Ok(code) => OperationResult {
            ok: true,
            cancelled,
            report_url: Some(format!("{}/reports/{code}", ctx.base_url.as_str().trim_end_matches('/'))),
            report_code: Some(code),
            error: None,
        },
        Err(operation::Error::Cancelled) => OperationResult {
            ok: false,
            cancelled: true,
            report_code: None,
            report_url: None,
            error: None,
        },
        Err(e) => {
            ctx.log(format!("Failed: {e}"));
            OperationResult { ok: false, cancelled, report_code: None, report_url: None, error: Some(e.to_string()) }
        }
    };
    let _ = app.emit("operation-finished", &out);
    out
}

#[tauri::command]
pub async fn start_upload(
    app: AppHandle,
    state: State<'_, AppState>,
    params: UploadParams,
) -> Result<OperationResult, String> {
    let ctx = begin_operation(&app, &state, "upload")?;
    let result = operation::upload_log(&ctx, params).await;
    Ok(finish_operation(&app, &state, &ctx, result))
}

#[tauri::command]
pub async fn start_live_log(
    app: AppHandle,
    state: State<'_, AppState>,
    params: LiveParams,
) -> Result<OperationResult, String> {
    let ctx = begin_operation(&app, &state, "live")?;
    let result = operation::live_log(&ctx, params).await;
    Ok(finish_operation(&app, &state, &ctx, result))
}

#[tauri::command]
pub async fn cancel_operation(state: State<'_, AppState>) -> Result<bool, String> {
    let slot = state.operation.lock().unwrap();
    match slot.as_ref() {
        Some(cancel) => {
            cancel.store(true, Ordering::Relaxed);
            Ok(true)
        }
        None => Ok(false),
    }
}

#[tauri::command]
pub async fn parser_loaded(state: State<'_, AppState>) -> Result<(), String> {
    state.parser.mark_loaded();
    Ok(())
}

#[tauri::command]
pub async fn parser_failed(state: State<'_, AppState>, message: String) -> Result<(), String> {
    state.parser.mark_failed(message);
    Ok(())
}

#[tauri::command]
pub async fn parser_response(
    state: State<'_, AppState>,
    request_id: u64,
    payload: Option<Value>,
    error: Option<String>,
) -> Result<(), String> {
    let reply = match (payload, error) {
        (_, Some(err)) => Err(err),
        (Some(value), None) => Ok(value),
        (None, None) => Err("empty reply".to_string()),
    };
    state.parser.resolve(request_id, reply);
    Ok(())
}

#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http URL".into());
    }
    open::that(url).map_err(|e| e.to_string())
}
