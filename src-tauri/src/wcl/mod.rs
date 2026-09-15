//! HTTP client for the Warcraft Logs `desktop-client` API.
//!
//! See `docs/PROTOCOL.md` for the wire format. All requests ride on the
//! `wcl_session` cookie obtained from [`Client::login`].

use std::sync::Arc;

use reqwest::cookie::{CookieStore, Jar};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Version string reported to the server. The server enforces a minimum
/// client version, so this tracks the official uploader release the protocol
/// was reconstructed from.
pub const CLIENT_VERSION: &str = "9.6.43";

/// User-Agent the official (Electron) uploader sends. The server returns 404
/// for `/desktop-client/parser` unless the request looks like it; both the
/// webview (which loads the parser iframe) and this client use it.
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) archon/9.6.43 Chrome/142.0.7444.175 Electron/39.8.10 Safari/537.36";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{message}")]
    Server { status: u16, message: String },
    #[error("unexpected response: {0}")]
    Malformed(String),
}

impl Error {
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Server { status, .. } => Some(*status),
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            Error::Malformed(_) => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One entry of a `*SelectItems` list from the login response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectItem {
    pub value: Value,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_id: Option<Value>,
}

/// Treat an explicit JSON `null` like a missing field.
fn null_as_default<'de, D, T>(de: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

/// The parts of the login response the uploader needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub id: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    pub guild_select_items: Vec<SelectItem>,
    /// Keyed by guild id (as a string, because it is a JSON object key).
    #[serde(default, deserialize_with = "null_as_default")]
    pub report_tag_select_items: serde_json::Map<String, Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub report_visibility_select_items: Vec<SelectItem>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub region_or_server_select_items: Vec<SelectItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReport {
    pub client_version: String,
    pub parser_version: Value,
    pub start_time: i64,
    pub end_time: i64,
    pub guild_id: Value,
    pub file_name: String,
    pub server_or_region: Value,
    pub visibility: Value,
    pub report_tag_id: Value,
    pub description: String,
    /// 1 = live log, 2 = upload a log, 3 = auto log
    pub log_mode: u8,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentParameters {
    pub start_time: Value,
    pub end_time: Value,
    pub mythic: Value,
    pub is_live_log: bool,
    pub is_real_time: bool,
    pub in_progress_event_count: i64,
    pub segment_id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SegmentResponse {
    #[serde(default)]
    next_segment_id: i64,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParserCode {
    pub gamedata_code: String,
    pub parser_code: String,
    pub parser_version: Value,
    pub bundle_url: String,
}

/// The inline `<script>` that sets `window.gameContentTypes`.
fn extract_gamedata(html: &str) -> Option<String> {
    for cap in script_bodies(html) {
        if cap.contains("gameContentTypes") {
            return Some(cap.to_string());
        }
    }
    None
}

/// The external parser bundle, e.g.
/// `https://assets.rpglogs.com/js/parser-warcraft.<hash>.js`.
fn extract_bundle_url(html: &str) -> Option<String> {
    let re = regex::Regex::new(
        r#"src="(https://assets\.rpglogs\.com/js/(?:[\w./-]+/)*parser-[\w.-]+\.js)""#,
    )
    .ok()?;
    re.captures(html).map(|c| c[1].to_string())
}

fn extract_parser_version(html: &str) -> Value {
    regex::Regex::new(r"parserVersion\s*=\s*(\d+)")
        .ok()
        .and_then(|re| re.captures(html))
        .and_then(|c| c[1].parse::<i64>().ok())
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// Yield the text of each `<script>...</script>` body in `html`.
fn script_bodies(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let mut i = 0;
    while let Some(open) = html[i..].find("<script") {
        let start = i + open;
        let Some(gt) = html[start..].find('>') else { break };
        let body_start = start + gt + 1;
        let Some(close) = html[body_start..].find("</script>") else { break };
        out.push(&html[body_start..body_start + close]);
        i = body_start + close + "</script>".len();
    }
    let _ = bytes;
    out
}

pub struct Client {
    http: reqwest::Client,
    jar: Arc<Jar>,
}

impl Client {
    pub fn new() -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http, jar })
    }

    #[allow(dead_code)]
    /// The `Cookie:` header value the jar would send to `base`, as
    /// `name=value` pairs. Used to mirror the session into the webview.
    pub fn cookie_pairs(&self, base: &Url) -> Vec<(String, String)> {
        let Some(header) = self.jar.cookies(base) else {
            return Vec::new();
        };
        header
            .to_str()
            .unwrap_or_default()
            .split(';')
            .filter_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                Some((name.to_string(), value.to_string()))
            })
            .collect()
    }

    fn endpoint(base: &Url, path: &str) -> Url {
        let mut url = base.clone();
        url.set_path(&format!("/desktop-client/{path}"));
        url
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<ErrorBody>(&text)
            .ok()
            .and_then(|b| b.message)
            .unwrap_or_else(|| {
                if text.is_empty() {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}: {}", text.chars().take(300).collect::<String>())
                }
            });
        Err(Error::Server { status: status.as_u16(), message })
    }

    pub async fn login(
        &self,
        base: &Url,
        email: &str,
        password: &str,
        game_version_id: &str,
    ) -> Result<UserInfo> {
        let body = serde_json::json!({
            "email": email,
            "password": password,
            "version": CLIENT_VERSION,
            "clientTime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "gameVersionId": game_version_id,
        });
        let resp = self
            .http
            .post(Self::endpoint(base, "log-in"))
            .json(&body)
            .send()
            .await?;
        let mut data: Value = Self::check(resp).await?.json().await?;

        // The client flattens `{ user: {...}, ...rest }` into one object.
        let merged = match data.as_object_mut() {
            Some(obj) => {
                let mut merged = obj.clone();
                if let Some(Value::Object(user)) = obj.remove("user") {
                    merged.remove("user");
                    merged.extend(user);
                }
                Value::Object(merged)
            }
            None => return Err(Error::Malformed("login response is not an object".into())),
        };
        if let Some(obj) = merged.as_object() {
            let shape: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    let ty = match v {
                        Value::Null => "null",
                        Value::Bool(_) => "bool",
                        Value::Number(_) => "number",
                        Value::String(_) => "string",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                    };
                    format!("{k}:{ty}")
                })
                .collect();
            log::debug!("login response shape: {}", shape.join(" "));
        }
        serde_json::from_value(merged).map_err(|e| Error::Malformed(format!("login response: {e}")))
    }

    /// Fetch the parser page and extract the code needed to run the parser
    /// out-of-browser: the inline "gamedata" script and the external
    /// `parser-<game>.<hash>.js` bundle. Requires an authenticated session.
    pub async fn fetch_parser_code(&self, base: &Url, parser_url: &str) -> Result<ParserCode> {
        let resp = self.http.get(parser_url).send().await?;
        let status = resp.status().as_u16();
        let html = resp.text().await.unwrap_or_default();
        log::debug!("fetch_parser_code: HTTP {status}, {} bytes", html.len());
        if status != 200 {
            return Err(Error::Server {
                status,
                message: format!("parser page returned HTTP {status}"),
            });
        }

        let gamedata_code = extract_gamedata(&html)
            .ok_or_else(|| Error::Malformed("no gamedata script in parser page".into()))?;
        let bundle_url = extract_bundle_url(&html)
            .ok_or_else(|| Error::Malformed("no parser bundle URL in parser page".into()))?;
        let parser_code = self.http.get(&bundle_url).send().await?.text().await?;
        let parser_version = extract_parser_version(&html);

        let _ = base; // reserved for future per-site handling
        Ok(ParserCode { gamedata_code, parser_code, parser_version, bundle_url })
    }

    /// GET an absolute URL with the session; returns (status, body prefix).
    /// Used to check the parser page is reachable before loading it.
    pub async fn probe(&self, url: &str) -> Result<(u16, String)> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    pub async fn logout(&self, base: &Url) -> Result<()> {
        let resp = self
            .http
            .post(Self::endpoint(base, "log-out"))
            .json(&serde_json::json!({}))
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    pub async fn create_report(&self, base: &Url, report: &CreateReport) -> Result<String> {
        let resp = self
            .http
            .post(Self::endpoint(base, "create-report"))
            .json(report)
            .send()
            .await?;
        let data: Value = Self::check(resp).await?.json().await?;
        data.get("code")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Malformed(format!("create-report returned {data}")))
    }

    pub async fn set_master_table(
        &self,
        base: &Url,
        code: &str,
        segment_id: i64,
        is_real_time: bool,
        zip: Vec<u8>,
        players: String,
    ) -> Result<()> {
        let form = Form::new()
            .text("segmentId", segment_id.to_string())
            .text("isRealTime", is_real_time.to_string())
            .part(
                "logfile",
                Part::bytes(zip).file_name("blob").mime_str("application/zip")?,
            )
            .text("players", players);
        let resp = self
            .http
            .post(Self::endpoint(base, &format!("set-report-master-table/{code}")))
            .header("Accept", "application/json")
            .multipart(form)
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    /// Returns the `nextSegmentId` the server wants for the following segment.
    pub async fn add_report_segment(
        &self,
        base: &Url,
        code: &str,
        zip: Vec<u8>,
        parameters: &SegmentParameters,
    ) -> Result<i64> {
        let form = Form::new()
            .part(
                "logfile",
                Part::bytes(zip).file_name("blob").mime_str("application/zip")?,
            )
            .text("parameters", serde_json::to_string(parameters).unwrap_or_default());
        let resp = self
            .http
            .post(Self::endpoint(base, &format!("add-report-segment/{code}")))
            .header("Accept", "application/json")
            .multipart(form)
            .send()
            .await?;
        let data: SegmentResponse = Self::check(resp).await?.json().await?;
        Ok(data.next_segment_id)
    }

    pub async fn terminate_report(&self, base: &Url, code: &str) -> Result<()> {
        let resp = self
            .http
            .post(Self::endpoint(base, &format!("terminate-report/{code}")))
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }
}
