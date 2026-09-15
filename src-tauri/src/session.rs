//! Login session state and mirroring of the HTTP session into the webview.

use tauri::webview::Cookie;
use tauri::{AppHandle, Manager};
use url::Url;

use crate::wcl;

#[derive(Debug, Clone)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub base_url: Url,
    pub game_version_id: String,
}

/// Registrable domain for a site host (`www.warcraftlogs.com` → `.warcraftlogs.com`).
fn cookie_domain(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 2 {
        format!(".{}.{}", labels[labels.len() - 2], labels[labels.len() - 1])
    } else {
        host.to_string()
    }
}

/// Copy the session cookies reqwest holds for `base` into the main webview so
/// the parser iframe (served by the same site) is authenticated.
pub fn sync_cookies(app: &AppHandle, wcl: &wcl::Client, base: &Url) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    let host = base.host_str().ok_or_else(|| "base URL has no host".to_string())?;
    let domain = cookie_domain(host);
    let pairs = wcl.cookie_pairs(base);
    if pairs.is_empty() {
        return Err("no session cookies to sync".into());
    }
    for (name, value) in pairs {
        let cookie = Cookie::parse(format!(
            "{name}={value}; Domain={domain}; Path=/; Secure; HttpOnly; SameSite=None"
        ))
        .map_err(|e| format!("cookie parse: {e}"))?;
        window
            .set_cookie(cookie)
            .map_err(|e| format!("set cookie: {e}"))?;
    }
    Ok(())
}

/// Remove the site's cookies from the webview on logout.
pub fn clear_cookies(app: &AppHandle, base: &Url) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if let Ok(cookies) = window.cookies_for_url(base.clone()) {
        for cookie in cookies {
            let _ = window.delete_cookie(cookie);
        }
    }
}
