//! Login session state.

use url::Url;

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
