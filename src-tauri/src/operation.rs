//! Upload orchestration: "upload a log" and "live log" (docs/PROTOCOL.md §3–4).

use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use url::Url;

use crate::logfile::{self, FileInfo, FilePart};
use crate::parser::{self, scalar, FightsResult, LogFilePosition, ParserBridge};
use crate::session::Credentials;
use crate::wcl::{self, CreateReport, SegmentParameters, CLIENT_VERSION};

const MAX_FILE_BYTES: u64 = 3_500_000_000;
const UPLOAD_ATTEMPTS: u32 = 60;
const UPLOAD_RETRY_DELAY: Duration = Duration::from_secs(30);
const LIVE_IDLE_THRESHOLD_MS: i64 = 120_000;
const LIVE_MAX_FILE_AGE_MS: i64 = 6 * 60 * 60 * 1000;
const LIVE_POLL: Duration = Duration::from_secs(1);
/// Real-time live logging reads larger parts, like the official client.
const REAL_TIME_MAX_LINES: usize = 50_000;
const REAL_TIME_MAX_BYTES: u64 = 80 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Wcl(#[from] wcl::Error),
    #[error(transparent)]
    Parser(#[from] parser::Error),
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportOptions {
    pub guild_id: Value,
    pub region_or_server_id: Value,
    pub visibility: Value,
    #[serde(default)]
    pub report_tag_id: Value,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadParams {
    pub file_path: String,
    /// Ignore any saved resume state and start a fresh report (finalizing the
    /// previous one for this file).
    #[serde(default)]
    pub new_report: bool,
    #[serde(flatten)]
    pub report: ReportOptions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveParams {
    pub directory_path: String,
    #[serde(flatten)]
    pub report: ReportOptions,
    #[serde(default)]
    pub include_entire_file: bool,
    #[serde(default)]
    pub real_time: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub kind: String,
    pub phase: String,
    pub report_code: Option<String>,
    pub current_file: Option<String>,
    pub file_read_percent: f64,
    pub lines_parsed: u64,
    pub segments_uploaded: u32,
    pub next_segment_id: i64,
    pub uploading_master_info: bool,
    pub uploading_fights: bool,
    pub fight_in_progress: bool,
    pub is_caught_up: bool,
    pub elapsed_ms: u64,
}

/// Everything an operation needs; one per running upload / live log.
pub struct Ctx {
    pub app: AppHandle,
    pub wcl: Arc<wcl::Client>,
    pub parser: Arc<ParserBridge>,
    pub base_url: Url,
    pub game_version_id: String,
    pub credentials: Credentials,
    pub cancel: Arc<AtomicBool>,
    progress: Mutex<Progress>,
    started: Instant,
}

struct PartOptions<'a> {
    region: &'a Value,
    raids: &'a [i64],
    is_live_log: bool,
    is_real_time: bool,
    push_fight_if_needed: bool,
    check_first_line: bool,
    /// Fights ending at/below this byte offset were already uploaded (resume):
    /// parse them for state but don't re-send them.
    resume_through: u64,
}

impl Ctx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app: AppHandle,
        wcl: Arc<wcl::Client>,
        parser: Arc<ParserBridge>,
        base_url: Url,
        game_version_id: String,
        credentials: Credentials,
        cancel: Arc<AtomicBool>,
        kind: &str,
    ) -> Self {
        Self {
            app,
            wcl,
            parser,
            base_url,
            game_version_id,
            credentials,
            cancel,
            // The official client numbers segments from 1.
            progress: Mutex::new(Progress { kind: kind.to_string(), next_segment_id: 1, ..Default::default() }),
            started: Instant::now(),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn progress(&self, update: impl FnOnce(&mut Progress)) {
        let snapshot = {
            let mut p = self.progress.lock().unwrap();
            update(&mut p);
            p.elapsed_ms = self.started.elapsed().as_millis() as u64;
            p.clone()
        };
        let _ = self.app.emit("operation-progress", snapshot);
    }

    fn phase(&self, phase: &str) {
        self.progress(|p| p.phase = phase.to_string());
    }

    fn set_next_segment_id(&self, id: i64) {
        self.progress.lock().unwrap().next_segment_id = id;
    }

    fn next_segment_id(&self) -> i64 {
        self.progress.lock().unwrap().next_segment_id
    }

    pub fn log(&self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        let _ = self.app.emit("app-log", serde_json::json!({ "message": message }));
    }

    async fn ensure_parser(&self) -> Result<()> {
        if self.parser.is_ready().await {
            return Ok(());
        }
        self.phase("loading-parser");
        let url = parser::parser_url(self.base_url.as_str(), &self.game_version_id);
        self.log("Fetching parser from the server…");
        let code = self.wcl.fetch_parser_code(&self.base_url, &url).await?;
        let version = self.parser.start(&self.app, &code.gamedata_code, &code.parser_code, code.parser_version.clone()).await?;
        self.log(format!("Parser ready (version {})", parser::scalar(&version)));
        Ok(())
    }

    async fn relogin(&self) -> Result<()> {
        self.log("Session expired, logging in again");
        self.wcl
            .login(
                &self.base_url,
                &self.credentials.email,
                &self.credentials.password,
                &self.game_version_id,
            )
            .await?;
        Ok(())
    }

    /// Retry `op` on transient failures; on `401` log in again once.
    async fn with_retry<T, F, Fut>(&self, what: &str, op: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = wcl::Result<T>>,
    {
        let mut attempts_left = UPLOAD_ATTEMPTS;
        let mut relogged = false;
        loop {
            attempts_left -= 1;
            let err = match op().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            self.check_cancelled()?;
            if attempts_left == 0 {
                return Err(err.into());
            }
            match err.status() {
                Some(401) => {
                    if relogged {
                        return Err(err.into());
                    }
                    self.relogin().await?;
                    relogged = true;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Some(s) if (400..500).contains(&s) && s != 408 && s != 429 => {
                    return Err(err.into());
                }
                _ => {
                    self.log(format!(
                        "{what} failed ({err}); retrying in {}s",
                        UPLOAD_RETRY_DELAY.as_secs()
                    ));
                    tokio::time::sleep(UPLOAD_RETRY_DELAY).await;
                }
            }
        }
    }

    async fn create_report(&self, file_name: &str, opts: &ReportOptions, log_mode: u8) -> Result<String> {
        self.phase("creating-report");
        let parser_version = self.parser.get_version(&self.app).await?;
        let now = chrono::Utc::now().timestamp_millis();
        let report = CreateReport {
            client_version: CLIENT_VERSION.to_string(),
            parser_version,
            start_time: now,
            end_time: now,
            guild_id: opts.guild_id.clone(),
            file_name: file_name.to_string(),
            server_or_region: opts.region_or_server_id.clone(),
            visibility: opts.visibility.clone(),
            report_tag_id: opts.report_tag_id.clone(),
            description: opts.description.clone(),
            log_mode,
        };
        let code = self.wcl.create_report(&self.base_url, &report).await?;
        self.log(format!("Report code: {code}"));
        self.progress(|p| p.report_code = Some(code.clone()));
        self.parser.set_report_code(&self.app, &code).await?;
        Ok(code)
    }

    async fn terminate_report(&self, code: &str) {
        self.phase("finishing");
        self.parser.reset(&self.app).await;
        if let Err(e) = self.wcl.terminate_report(&self.base_url, code).await {
            self.log(format!("terminate-report failed: {e}"));
        }
    }

    fn master_table_text(info: &parser::MasterInfo, fights: &FightsResult) -> String {
        let mut text = format!(
            "{}|{}|{}\n",
            scalar(&fights.log_version),
            scalar(&fights.game_version),
            scalar(&fights.log_file_details)
        );
        text.push_str(&format!("{}\n", scalar(&info.last_assigned_actor_id)));
        text.push_str(&info.actors_string);
        text.push_str(&format!("{}\n", scalar(&info.last_assigned_ability_id)));
        text.push_str(&info.abilities_string);
        text.push_str(&format!("{}\n", scalar(&info.last_assigned_tuple_id)));
        text.push_str(&info.tuples_string);
        text.push_str(&format!("{}\n", scalar(&info.last_assigned_pet_id)));
        text.push_str(&info.pets_string);
        text
    }

    fn fights_text(fights: &FightsResult) -> String {
        let total: i64 = fights.fights.iter().map(|f| f.event_count).sum();
        let mut text = format!(
            "{}|{}\n{}\n",
            scalar(&fights.log_version),
            scalar(&fights.game_version),
            total
        );
        for f in &fights.fights {
            text.push_str(&f.events_string);
        }
        text
    }

    /// Feed one file part to the parser and upload whatever fights it
    /// completes. Returns whether a segment was uploaded.
    async fn upload_file_part(
        &self,
        file: &FileInfo,
        part: &FilePart,
        code: &str,
        opts: &PartOptions<'_>,
    ) -> Result<bool> {
        if opts.check_first_line {
            if let Some(first) = part.lines.first() {
                if !first.trim().is_empty() && !logfile::has_timestamp(first) {
                    return Err(Error::Message(format!(
                        "This does not look like a combat log. First line: {}",
                        first.chars().take(120).collect::<String>()
                    )));
                }
            }
        }
        if part.lines.is_empty() && !opts.push_fight_if_needed {
            return Ok(false);
        }

        self.progress(|p| {
            p.phase = "parsing".into();
            p.current_file = Some(file.file_name.clone());
            p.file_read_percent = if file.size > 0 {
                part.current_position as f64 / file.size as f64 * 100.0
            } else {
                0.0
            };
        });

        if !part.lines.is_empty() {
            let position = LogFilePosition {
                file_path: file.file_path.clone(),
                current_position: part.current_position,
                starting_position: part.starting_position,
            };
            self.parser
                .parse_lines(&self.app, &part.lines, opts.region, opts.raids, false, &position)
                .await?;
            self.progress(|p| p.lines_parsed += part.lines.len() as u64);
        }

        let push = opts.push_fight_if_needed || (part.end_of_file && !opts.is_live_log);
        let mut fights = self.parser.collect_fights(&self.app, push, false).await?;
        let mut in_progress = false;
        if opts.is_live_log && fights.fights.is_empty() {
            let partial = self.parser.collect_in_progress_fight(&self.app).await?;
            in_progress = !partial.fights.is_empty();
            self.progress(|p| p.fight_in_progress = in_progress);
            if opts.is_real_time && in_progress {
                fights = partial;
            }
        }
        if fights.fights.is_empty() {
            return Ok(false);
        }

        // Resume: this region was already uploaded — parsed for state, but skip re-sending.
        if opts.resume_through > 0 && part.current_position <= opts.resume_through {
            self.parser.clear_fights(&self.app).await?;
            return Ok(false);
        }

        let info = self.parser.collect_master_info(&self.app, code).await?;
        if !info.success {
            self.parser.reset(&self.app).await;
            return Err(Error::Message(format!(
                "Invalid parser state detected (expected report {}, got {}). Please re-upload the log.",
                info.expected_report_code.as_deref().unwrap_or("undefined"),
                info.actual_report_code.as_deref().unwrap_or("undefined")
            )));
        }

        let segment_id = self.progress.lock().unwrap().next_segment_id;

        let master_zip = logfile::zip_text(&Self::master_table_text(&info, &fights))?;
        self.progress(|p| {
            p.phase = "uploading".into();
            p.uploading_master_info = true;
        });
        let players = info.players_string.clone();
        self.with_retry("master table upload", || {
            self.wcl.set_master_table(
                &self.base_url,
                code,
                segment_id,
                opts.is_real_time,
                master_zip.clone(),
                players.clone(),
            )
        })
        .await?;
        self.progress(|p| p.uploading_master_info = false);

        let fights_zip = logfile::zip_text(&Self::fights_text(&fights))?;
        let parameters = SegmentParameters {
            start_time: fights.start_time.clone(),
            end_time: fights.end_time.clone(),
            mythic: fights.mythic.clone(),
            is_live_log: opts.is_live_log,
            is_real_time: opts.is_real_time,
            in_progress_event_count: if opts.is_real_time && in_progress {
                fights.fights.first().map(|f| f.event_count).unwrap_or(0)
            } else {
                0
            },
            segment_id,
        };
        self.progress(|p| p.uploading_fights = true);
        let next = self
            .with_retry("segment upload", || {
                self.wcl
                    .add_report_segment(&self.base_url, code, fights_zip.clone(), &parameters)
            })
            .await?;
        self.progress(|p| {
            p.uploading_fights = false;
            p.segments_uploaded += 1;
            if next > 0 {
                p.next_segment_id = next;
            }
        });
        self.log(format!(
            "Uploaded segment {segment_id} ({} fight(s), {} events)",
            fights.fights.len(),
            fights.fights.iter().map(|f| f.event_count).sum::<i64>()
        ));

        self.parser.clear_fights(&self.app).await?;
        Ok(true)
    }
}

/// Upload a finished combat log file. Returns the report code.
pub async fn upload_log(ctx: &Ctx, params: UploadParams) -> Result<String> {
    ctx.ensure_parser().await?;
    ctx.parser.reset(&ctx.app).await;

    let path = Path::new(&params.file_path);
    let file = logfile::file_info(path)?;
    if file.size > MAX_FILE_BYTES {
        return Err(Error::Message("The log file is too large to upload (max 3.5 GB).".into()));
    }
    ctx.log(format!("Reading file: {} ({} bytes)", file.file_name, file.size));

    // Resume a prior upload of this same growing file, if we have valid state
    // for it on this site (skip everything already sent).
    let base_url = ctx.base_url.as_str().to_string();
    let existing = crate::upload_state::get(&ctx.app, &file.file_path);
    if params.new_report {
        if let Some(old) = &existing {
            ctx.log(format!("Finalizing previous report {}", old.report_code));
            ctx.terminate_report(&old.report_code).await;
        }
        crate::upload_state::clear(&ctx.app, &file.file_path);
    }
    let saved = existing.filter(|e| {
        !params.new_report && e.base_url == base_url && e.position > 0 && file.size >= e.position
    });

    let (code, resume_through) = if let Some(entry) = saved {
        ctx.parser.set_report_code(&ctx.app, &entry.report_code).await?;
        ctx.set_next_segment_id(entry.next_segment_id);
        ctx.log(format!(
            "Resuming report {} — {} MB already uploaded; sending only new fights",
            entry.report_code,
            entry.position / 1_000_000
        ));
        (entry.report_code, entry.position)
    } else {
        (ctx.create_report(&file.file_name, &params.report, 2).await?, 0)
    };

    let result: Result<()> = async {
        let mut position = 0u64;
        let mut first = true;
        loop {
            ctx.check_cancelled()?;
            let part = logfile::read_file_part(path, position, logfile::MAX_LINES, logfile::MAX_BYTES, true)?;
            position = part.current_position;
            let opts = PartOptions {
                region: &params.report.region_or_server_id,
                raids: &[],
                is_live_log: false,
                is_real_time: false,
                push_fight_if_needed: part.end_of_file,
                check_first_line: first,
                resume_through,
            };
            let uploaded = ctx.upload_file_part(&file, &part, &code, &opts).await?;
            if uploaded {
                crate::upload_state::set(&ctx.app, &file.file_path, crate::upload_state::Entry {
                    report_code: code.clone(),
                    position: part.current_position,
                    next_segment_id: ctx.next_segment_id(),
                    base_url: base_url.clone(),
                });
            }
            first = false;
            if part.end_of_file {
                break;
            }
        }
        Ok(())
    }
    .await;

    // Intentionally not terminated: the same growing log file can be uploaded
    // again later to append its new fights to this report (see resume above).
    // The report is still fully viewable and processed in the meantime.
    result?;
    Ok(code)
}

/// Tail the newest combat log in a directory until cancelled. Returns the
/// report code.
pub async fn live_log(ctx: &Ctx, params: LiveParams) -> Result<String> {
    ctx.ensure_parser().await?;
    ctx.parser.reset(&ctx.app).await;

    let dir = Path::new(&params.directory_path);
    if !dir.is_dir() {
        return Err(Error::Message(format!("Not a directory: {}", params.directory_path)));
    }
    let pattern = Regex::new(crate::game::LOG_FILE_PATTERN).expect("valid pattern");
    let (max_lines, max_bytes) = if params.real_time {
        (REAL_TIME_MAX_LINES, REAL_TIME_MAX_BYTES)
    } else {
        (logfile::MAX_LINES, logfile::MAX_BYTES)
    };

    let code = ctx.create_report("live.log", &params.report, 1).await?;

    let result: Result<()> = async {
        let region = &params.report.region_or_server_id;
        let mut current: Option<FileInfo> = None;
        let mut position = 0u64;

        // Catch up on the file that already exists, if any.
        if let Some(file) = logfile::latest_file_in_dir(dir, &pattern, LIVE_MAX_FILE_AGE_MS) {
            if !params.include_entire_file {
                ctx.parser
                    .set_live_logging_start_time(&ctx.app, chrono::Utc::now().timestamp_millis())
                    .await?;
                position = file.size;
                if position > 0 {
                    let headers = logfile::header_lines_up_to(file.path(), position)?;
                    if !headers.is_empty() {
                        ctx.log(format!("Priming parser with {} header line(s)", headers.len()));
                        let pos = LogFilePosition {
                            file_path: file.file_path.clone(),
                            current_position: position,
                            starting_position: 0,
                        };
                        ctx.parser.parse_lines(&ctx.app, &headers, region, &[], false, &pos).await?;
                    }
                }
            }
            ctx.log(format!("Live logging {} from byte {position}", file.file_name));
            ctx.progress(|p| p.current_file = Some(file.file_name.clone()));
            loop {
                ctx.check_cancelled()?;
                let part = logfile::read_file_part(file.path(), position, max_lines, max_bytes, false)?;
                position = part.current_position;
                let opts = PartOptions {
                    region,
                    raids: &[],
                    is_live_log: true,
                    is_real_time: false,
                    push_fight_if_needed: false,
                    check_first_line: false,
                    resume_through: 0,
                };
                ctx.upload_file_part(&file, &part, &code, &opts).await?;
                if part.end_of_file || part.lines.is_empty() {
                    break;
                }
            }
            current = Some(file);
        }
        ctx.progress(|p| {
            p.is_caught_up = true;
            p.phase = "watching".into();
        });
        ctx.log("Caught up; watching for new events");

        // Tail loop.
        loop {
            if ctx.cancelled() {
                if let Some(file) = &current {
                    let part = FilePart {
                        lines: Vec::new(),
                        starting_position: position,
                        current_position: file.size,
                        end_of_file: true,
                    };
                    let opts = PartOptions {
                        region,
                        raids: &[],
                        is_live_log: true,
                        is_real_time: params.real_time,
                        push_fight_if_needed: true,
                        check_first_line: false,
                        resume_through: 0,
                    };
                    let _ = ctx.upload_file_part(file, &part, &code, &opts).await;
                }
                return Ok(());
            }

            let Some(latest) = logfile::latest_file_in_dir(dir, &pattern, LIVE_MAX_FILE_AGE_MS)
                .filter(|f| f.size > 0)
            else {
                tokio::time::sleep(LIVE_POLL).await;
                continue;
            };

            let changed = current.as_ref().map(|c| c.file_path != latest.file_path).unwrap_or(true);
            let truncated = !changed && current.as_ref().map(|c| latest.size < c.size).unwrap_or(false);
            let grew = !changed && current.as_ref().map(|c| latest.size > c.size).unwrap_or(false);

            if truncated {
                ctx.log("Detected possible truncation, rewinding to 0");
                position = 0;
            } else if changed {
                // Drain what is left of the previous file before switching.
                if let Some(old) = current.as_ref() {
                    if let Ok(old_now) = logfile::file_info(old.path()) {
                        if old_now.size > position {
                            drain(ctx, &old_now, &mut position, &code, region, &params, max_lines, max_bytes).await?;
                        }
                    }
                }
                position = 0;
                ctx.log(format!("Log file changed: {} ({} bytes)", latest.file_name, latest.size));
                ctx.progress(|p| p.current_file = Some(latest.file_name.clone()));
            } else if !grew {
                tokio::time::sleep(LIVE_POLL).await;
                continue;
            }

            drain(ctx, &latest, &mut position, &code, region, &params, max_lines, max_bytes).await?;
            current = Some(latest);
        }
    }
    .await;

    ctx.terminate_report(&code).await;
    result?;
    Ok(code)
}

/// Read and upload everything available in `file` from `position` onwards.
#[allow(clippy::too_many_arguments)]
async fn drain(
    ctx: &Ctx,
    file: &FileInfo,
    position: &mut u64,
    code: &str,
    region: &Value,
    params: &LiveParams,
    max_lines: usize,
    max_bytes: u64,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    loop {
        let part = logfile::read_file_part(file.path(), *position, max_lines, max_bytes, false)?;
        *position = part.current_position;
        let idle = now - file.last_modified_ms > LIVE_IDLE_THRESHOLD_MS;
        let opts = PartOptions {
            region,
            raids: &[],
            is_live_log: true,
            is_real_time: params.real_time,
            push_fight_if_needed: ctx.cancelled() || idle,
            check_first_line: false,
            resume_through: 0,
        };
        ctx.upload_file_part(file, &part, code, &opts).await?;
        if part.end_of_file || part.lines.is_empty() {
            return Ok(());
        }
    }
}
