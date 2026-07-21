//! HTTP client for the unofficial Libro.fm mobile API.
//!
//! Endpoints and headers follow community clients
//! ([jedwards1230/libro-client](https://github.com/jedwards1230/libro-client),
//! [burntcookie90/librofm-downloader](https://github.com/burntcookie90/librofm-downloader),
//! [bfordham/librofm](https://codeberg.org/bfordham/librofm)) and the notes in
//! audiobookshelf [#2112](https://github.com/advplyr/audiobookshelf/issues/2112).

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::{LibroError, Result};

/// Default API origin.
///
/// TODO: Confirm if regional mirrors exist; community tools all use this host.
pub const DEFAULT_BASE_URL: &str = "https://libro.fm";

/// OAuth password-grant path.
pub const OAUTH_TOKEN_PATH: &str = "/oauth/token";

/// Paginated library listing.
///
/// TODO: Some newer clients use `/api/v10/library` — bump if v7 is retired.
pub const LIBRARY_PATH: &str = "/api/v7/library";

/// DRM-free MP3 part manifest (zip URLs).
///
/// TODO: Some newer clients use `/api/v10/download-manifest`.
pub const DOWNLOAD_MANIFEST_PATH: &str = "/api/v9/download-manifest";

/// Packaged single-file M4B when Libro.fm offers it.
pub const PACKAGED_M4B_PATH: &str = "/api/v10/audiobooks/{isbn}/packaged_m4b";

/// Android app version header (`X-LibroFm-AppVer`).
///
/// TODO: Keep in sync with librofm-downloader `LIBRO_FM_HEADERS` / Android releases.
pub const APP_VER: &str = "7.34.8";

/// User-Agent matching the official Android HTTP stack.
pub const USER_AGENT_VALUE: &str = "okhttp/5.3.2";

/// Optional OAuth `client_id`.
///
/// TODO: Historical / reverse-engineered clients sometimes sent a mobile
/// `client_id` with form-encoded password grants. Current community tools
/// (libro-client, librofm-downloader, bfordham/librofm) omit it and POST JSON.
/// Set a non-empty value here if Libro.fm starts requiring it again.
pub const CLIENT_ID: &str = "";

/// Authenticated Libro.fm HTTP helper.
#[derive(Debug, Clone)]
pub struct LibroClient {
    http: Client,
    base_url: String,
    access_token: Option<String>,
}

impl Default for LibroClient {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_URL)
    }
}

impl LibroClient {
    /// Build a client pointed at `base_url` (no trailing slash).
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: Client::new(),
            base_url,
            access_token: None,
        }
    }

    /// Override the HTTP client (tests / custom timeouts).
    #[must_use]
    pub fn with_http(mut self, http: Client) -> Self {
        self.http = http;
        self
    }

    /// Attach a bearer token for subsequent requests.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// Current bearer token, if any.
    #[must_use]
    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    /// Base URL in use.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn headers(&self, with_auth: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert("X-LibroFm-AppVer", HeaderValue::from_static(APP_VER));
        if with_auth {
            let token = self
                .access_token
                .as_deref()
                .ok_or_else(|| LibroError::auth("not logged in"))?;
            let value = format!("Bearer {token}");
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&value)
                    .map_err(|e| LibroError::auth(format!("invalid token header: {e}")))?,
            );
        }
        Ok(headers)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Password-grant login. Returns token metadata and stores the access token.
    pub async fn login(&mut self, email: &str, password: &str) -> Result<TokenResponse> {
        let mut body = serde_json::json!({
            "grant_type": "password",
            "username": email,
            "password": password,
        });
        if !CLIENT_ID.is_empty() {
            body["client_id"] = serde_json::Value::String(CLIENT_ID.to_string());
        }

        let resp = self
            .http
            .post(self.url(OAUTH_TOKEN_PATH))
            .headers(self.headers(false)?)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(LibroError::auth(format!(
                "oauth/token failed ({status}): {text}"
            )));
        }

        let token: TokenResponse = serde_json::from_str(&text).map_err(|e| {
            LibroError::auth(format!("oauth/token decode failed: {e}; body={text}"))
        })?;
        if token.access_token.is_empty() {
            return Err(LibroError::auth("oauth/token returned empty access_token"));
        }
        if let Some(err) = token.error.as_deref() {
            return Err(LibroError::auth(err.to_string()));
        }

        self.access_token = Some(token.access_token.clone());
        Ok(token)
    }

    /// Fetch one library page (`page` is 1-based).
    pub async fn library_page(&self, page: u32) -> Result<LibraryPage> {
        let resp = self
            .http
            .get(self.url(LIBRARY_PATH))
            .headers(self.headers(true)?)
            .query(&[("page", page)])
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// Download-manifest for MP3 zip parts.
    pub async fn download_manifest(&self, isbn: &str) -> Result<DownloadManifest> {
        let resp = self
            .http
            .get(self.url(DOWNLOAD_MANIFEST_PATH))
            .headers(self.headers(true)?)
            .query(&[("isbn", isbn)])
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// Packaged M4B metadata when available (`None` on 404 / missing URL).
    pub async fn packaged_m4b(&self, isbn: &str) -> Result<Option<PackagedM4b>> {
        let path = PACKAGED_M4B_PATH.replace("{isbn}", isbn);
        let resp = self
            .http
            .get(self.url(&path))
            .headers(self.headers(true)?)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // Some deployments return 200 with empty/`{}` when unavailable.
            if status.is_client_error() {
                return Ok(None);
            }
            return Err(LibroError::api(format!(
                "packaged_m4b failed ({status}): {text}"
            )));
        }
        let text = resp.text().await?;
        if text.trim().is_empty() || text.trim() == "{}" {
            return Ok(None);
        }
        let meta: PackagedM4b = serde_json::from_str(&text).map_err(|e| {
            LibroError::api(format!("packaged_m4b decode failed: {e}; body={text}"))
        })?;
        if meta.m4b_url.is_empty() {
            return Ok(None);
        }
        Ok(Some(meta))
    }

    /// Download arbitrary URL bytes (CDN part or M4B). Sends auth headers for libro.fm hosts.
    pub async fn download_bytes(&self, url: &str) -> Result<bytes::Bytes> {
        let mut req = self.http.get(url);
        if url_is_libro_host(url) {
            req = req.headers(self.headers(true)?);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LibroError::download(format!("GET {url} failed ({status})")));
        }
        Ok(resp.bytes().await?)
    }

    async fn json_or_error<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(LibroError::api(format!("HTTP {status}: {text}")));
        }
        if let Ok(err) = serde_json::from_str::<ApiErrorBody>(&text) {
            if let Some(msg) = err.error.or(err.message) {
                return Err(LibroError::api(msg));
            }
        }
        serde_json::from_str(&text)
            .map_err(|e| LibroError::api(format!("JSON decode failed: {e}; body={text}")))
    }
}

fn url_is_libro_host(url: &str) -> bool {
    // Avoid a hard dependency on the `url` crate — parse host between scheme and path/query.
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "libro.fm" || host.ends_with(".libro.fm")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default = "default_bearer")]
    pub token_type: String,
    /// Unix seconds when the token was issued (mobile API).
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

fn default_bearer() -> String {
    String::from("Bearer")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibraryPage {
    #[serde(default)]
    pub page: u32,
    #[serde(default)]
    pub total_pages: u32,
    #[serde(default)]
    pub audiobooks: Vec<Audiobook>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Audiobook {
    /// ISBN may arrive as string or number in JSON.
    #[serde(deserialize_with = "deserialize_isbn")]
    pub isbn: String,
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_authors")]
    pub authors: Option<Vec<String>>,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub publication_date: Option<String>,
    #[serde(default)]
    pub abridged: Option<bool>,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub series_num: Option<serde_json::Value>,
    #[serde(default)]
    pub genres: Option<Vec<Genre>>,
    #[serde(default)]
    pub audiobook_info: Option<AudiobookInfo>,
    #[serde(default)]
    pub user_metadata: Option<UserMetadata>,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Genre {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudiobookInfo {
    #[serde(default)]
    pub narrators: Option<Vec<String>>,
    /// Duration in seconds.
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub parts_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserMetadata {
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub finished: Option<bool>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadManifest {
    #[serde(default, deserialize_with = "deserialize_optional_isbn")]
    pub isbn: Option<String>,
    #[serde(default)]
    pub parts: Vec<DownloadPart>,
    #[serde(default)]
    pub tracks: Vec<ManifestTrack>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadPart {
    pub url: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManifestTrack {
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub length_sec: Option<u64>,
    #[serde(default)]
    pub chapter_title: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackagedM4b {
    pub m4b_url: String,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: Option<String>,
    message: Option<String>,
}

fn deserialize_isbn<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected isbn string or number, got {other}"
        ))),
    }
}

fn deserialize_optional_isbn<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected isbn string or number, got {other}"
        ))),
    }
}

fn deserialize_authors<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(vec![s])),
        Some(serde_json::Value::Array(arr)) => {
            let mut out = Vec::new();
            for item in arr {
                match item {
                    serde_json::Value::String(s) => out.push(s),
                    serde_json::Value::Object(map) => {
                        if let Some(serde_json::Value::String(name)) = map.get("name") {
                            out.push(name.clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(out).filter(|v| !v.is_empty()))
        }
        Some(_) => Ok(None),
    }
}
