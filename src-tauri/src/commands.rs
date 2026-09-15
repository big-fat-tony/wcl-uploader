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
use crate::session::{Credentials, Session};
use crate::settings::{self, Settings};
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

async fn do_login(
    app: &AppHandle,
    state: &AppState,
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
    Ok(LoginResult { game_version_id, base_url: gv.base_url.to_string(), user })
}

#[tauri::command]
pub async fn login(
    app: AppHandle,
    state: State<'_, AppState>,
    email: String,
    password: String,
    game_version_id: String,
    remember_password: bool,
) -> Result<LoginResult, String> {
    let result = do_login(&app, &state, email.clone(), password.clone(), game_version_id.clone()).await?;
    let mut settings = settings::load(&app);
    if remember_password {
        settings::store_password(&email, &password)?;
    } else {
        settings::delete_password(&email);
    }
    settings.email = email;
    settings.remember_password = remember_password;
    settings.game_version_id = game_version_id;
    settings::save(&app, &settings)?;
    Ok(result)
}

/// Log in with the credentials saved by a previous "remember password" login.
#[tauri::command]
pub async fn auto_login(app: AppHandle, state: State<'_, AppState>) -> Result<Option<LoginResult>, String> {
    let settings = settings::load(&app);
    if !settings.remember_password || settings.email.is_empty() {
        return Ok(None);
    }
    let Some(password) = settings::load_password(&settings.email) else {
        return Ok(None);
    };
    let game_version_id = if settings.game_version_id.is_empty() {
        "warcraft-live".to_string()
    } else {
        settings.game_version_id.clone()
    };
    do_login(&app, &state, settings.email.clone(), password, game_version_id).await.map(Some)
}

#[tauri::command]
pub async fn get_settings(app: AppHandle) -> Settings {
    settings::load(&app)
}

/// Persist UI settings. The password is never part of this payload.
#[tauri::command]
pub async fn save_settings(app: AppHandle, patch: Settings) -> Result<(), String> {
    let mut current = settings::load(&app);
    current.report = patch.report.or(current.report);
    if !patch.live_directory.is_empty() {
        current.live_directory = patch.live_directory;
    }
    current.include_entire_file = patch.include_entire_file.or(current.include_entire_file);
    current.real_time = patch.real_time.or(current.real_time);
    settings::save(&app, &current)
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    /// `--upload <file>`: auto-login, upload, print the result, exit.
    pub upload_file: Option<String>,
}

#[tauri::command]
pub async fn get_launch_options() -> LaunchOptions {
    let args: Vec<String> = std::env::args().collect();
    let upload_file = args
        .iter()
        .position(|a| a == "--upload")
        .and_then(|i| args.get(i + 1).cloned());
    LaunchOptions { upload_file }
}

#[tauri::command]
pub async fn exit_app(app: AppHandle, code: i32) {
    app.exit(code);
}

#[tauri::command]
pub async fn logout(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let session = state.session.lock().unwrap().take();
    if let Some(creds) = state.credentials.lock().unwrap().take() {
        settings::delete_password(&creds.email);
    }
    let mut settings = settings::load(&app);
    settings.remember_password = false;
    let _ = settings::save(&app, &settings);
    state.parser.shutdown().await;
    if let Some(session) = session {
        let _ = state.wcl.logout(&session.base_url).await;
    }
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
    log::info!(
        "operation finished: ok={} cancelled={} report={:?} error={:?}",
        out.ok, out.cancelled, out.report_url, out.error
    );
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
pub async fn ui_log(message: String) {
    log::info!("[ui] {message}");
}

/// Fetch the parser and run one line through the Node harness. Returns the
/// parser version on success; used to validate the pipeline without uploading.
#[tauri::command]
pub async fn parser_selftest(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let session = state.session.lock().unwrap().clone().ok_or("not logged in")?;
    let url = crate::parser::parser_url(session.base_url.as_str(), &session.game_version_id);
    let code = state.wcl.fetch_parser_code(&session.base_url, &url).await.map_err(|e| e.to_string())?;
    let version = state.parser.start(&app, &code.gamedata_code, &code.parser_code, code.parser_version.clone()).await.map_err(|e| e.to_string())?;
    let v = state.parser.get_version(&app).await.map_err(|e| e.to_string())?;
    Ok(format!("bundle={} version={} get_version={}", code.bundle_url, crate::parser::scalar(&version), crate::parser::scalar(&v)))
}

#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http URL".into());
    }
    open::that(url).map_err(|e| e.to_string())
}
