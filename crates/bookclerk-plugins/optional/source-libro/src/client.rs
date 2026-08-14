//! HTTP client for the unofficial Libro.fm mobile API.
//!
//! Endpoints and headers follow community clients
//! ([jedwards1230/libro-client](https://github.com/jedwards1230/libro-client),
//! [burntcookie90/librofm-downloader](https://github.com/burntcookie90/librofm-downloader),
//! [bfordham/librofm](https://codeberg.org/bfordham/librofm)) and the notes in
//! audiobookshelf [#2112](https://github.com/advplyr/audiobookshelf/issues/2112).
//!
//! Keep constants below in sync with the Android app via
//! `scripts/librofm-apk-probe/` (CI workflow `librofm-apk-probe.yml`).
//! Absolute `/api/vN/…` paths below are the **last extracted** prefix from the
//! Play Store APK — not a hard lock. When Libro.fm ships `v13` (etc.), the
//! probe reports drift and the sync PR rewrites these constants.

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};

use crate::error::{LibroError, Result};

/// Default API origin.
///
/// TODO: Confirm if regional mirrors exist; community tools all use this host.
pub const DEFAULT_BASE_URL: &str = "https://libro.fm";

/// OAuth password-grant path.
pub const OAUTH_TOKEN_PATH: &str = "/oauth/token";

/// Paginated library listing (`/api/vN/library` from the Android app prefix).
pub const LIBRARY_PATH: &str = "/api/v12/library";

/// Download manifest (`parts` + chapter `tracks`).
///
/// Android `DownloadApi.getDownloadManifest` sends `isbn`, `client_version`, and
/// optional `format` (`MediaFormat.M4B` → `"m4b"`; `ZIP` → omit / null).
pub const DOWNLOAD_MANIFEST_PATH: &str = "/api/v12/download-manifest";

/// Packaged single-file M4B when Libro.fm offers it.
pub const PACKAGED_M4B_PATH: &str = "/api/v12/audiobooks/{isbn}/packaged_m4b";

/// Android app version header (`X-LibroFm-AppVer`).
///
/// Keep in sync via `scripts/librofm-apk-probe/` / workflow `librofm-apk-probe.yml`.
pub const APP_VER: &str = "7.37.4";

/// User-Agent matching the official Android HTTP stack.
pub const USER_AGENT_VALUE: &str = "okhttp/4.12.0";

/// Android `MediaFormat` id for `download-manifest?format=…`.
///
/// - [`ManifestFormat::M4b`] → `format=m4b` (must be lowercase; `M4B` is ignored)
/// - [`ManifestFormat::Zip`] → omit `format` (APK `MediaFormat.ZIP` id is null)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ManifestFormat {
    /// Single-file M4B CDN URL in `parts` when the title supports it.
    M4b,
    /// Multi-part ZIP of MP3s (API default when `format` is omitted).
    #[default]
    Zip,
}

impl ManifestFormat {
    /// Wire value for the `format` query, if any.
    #[must_use]
    pub const fn query_value(self) -> Option<&'static str> {
        match self {
            Self::M4b => Some("m4b"),
            Self::Zip => None,
        }
    }
}

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
    /// Shared HTTP client (timeouts/tests override via [`LibroClient::with_http`]).
    http: reqwest::Client,
    /// API origin without a trailing slash (default [`DEFAULT_BASE_URL`]).
    base_url: String,
    /// Bearer token stored after login; required for authenticated routes.
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
            http: reqwest::Client::new(),
            base_url,
            access_token: None,
        }
    }

    /// Override the HTTP client (tests / custom timeouts).
    #[must_use]
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
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

    /// Builds Android-app headers; `with_auth` requires a stored bearer token.
    fn headers(&self, with_auth: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert("X-LibroFm-AppVer", HeaderValue::from_static(APP_VER));
        // Match Android AuthInterceptor (prod): device + OS version headers.
        headers.insert("X-LibroFm-Device", HeaderValue::from_static("bookclerk"));
        headers.insert("X-LibroFm-OsVer", HeaderValue::from_static("Android 34"));
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

    /// Joins `path` onto the configured origin.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Password-grant login. Returns token metadata and stores the access token.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
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

    /// Download-manifest for parts + chapter tracks.
    ///
    /// Pass [`ManifestFormat::M4b`] to request a single `.m4b` part (same asset as
    /// [`Self::packaged_m4b`]) plus tracks in one response. [`ManifestFormat::Zip`]
    /// (or omitting `format`) returns multi-part `.zip` URLs.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn download_manifest(
        &self,
        isbn: &str,
        format: ManifestFormat,
    ) -> Result<DownloadManifest> {
        let mut req = self
            .http
            .get(self.url(DOWNLOAD_MANIFEST_PATH))
            .headers(self.headers(true)?)
            .query(&[("isbn", isbn), ("client_version", APP_VER)]);
        if let Some(fmt) = format.query_value() {
            req = req.query(&[("format", fmt)]);
        }
        let resp = req.send().await?;
        Self::json_or_error(resp).await
    }

    /// Packaged M4B metadata when available (`None` on 404 / missing URL).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
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

    /// Decodes a JSON body, or maps HTTP / `{error,message}` payloads to [`LibroError`].
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

/// True when `url` is `libro.fm` or a subdomain (auth headers are then sent).
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

/// Token response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenResponse {
    /// Access token.
    pub access_token: String,
    /// Token type.
    #[serde(default = "default_bearer")]
    pub token_type: String,
    /// Unix seconds when the token was issued (mobile API).
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Expires in.
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Error.
    #[serde(default)]
    pub error: Option<String>,
}

/// Default `token_type` when the OAuth response omits it (`Bearer`).
fn default_bearer() -> String {
    String::from("Bearer")
}

/// Library page.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibraryPage {
    /// Page.
    #[serde(default)]
    pub page: u32,
    /// Total pages.
    #[serde(default)]
    pub total_pages: u32,
    /// Audiobooks.
    #[serde(default)]
    pub audiobooks: Vec<Audiobook>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Error.
    #[serde(default)]
    pub error: Option<String>,
}

/// Audiobook.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Audiobook {
    /// ISBN may arrive as string or number in JSON.
    #[serde(deserialize_with = "deserialize_isbn")]
    pub isbn: String,
    /// Title.
    pub title: String,
    /// Authors.
    #[serde(default, deserialize_with = "deserialize_authors")]
    pub authors: Option<Vec<String>>,
    /// Cover URL.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Subtitle.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Publisher.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Publication date.
    #[serde(default)]
    pub publication_date: Option<String>,
    /// Abridged.
    #[serde(default)]
    pub abridged: Option<bool>,
    /// Series.
    #[serde(default)]
    pub series: Option<String>,
    /// Series num.
    #[serde(default)]
    pub series_num: Option<serde_json::Value>,
    /// Genres.
    #[serde(default)]
    pub genres: Option<Vec<Genre>>,
    /// Audiobook info.
    #[serde(default)]
    pub audiobook_info: Option<AudiobookInfo>,
    /// User metadata.
    #[serde(default)]
    pub user_metadata: Option<UserMetadata>,
    /// Identifier.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Genre tag from a library-page audiobook (`name` may be missing).
pub struct Genre {
    #[serde(default)]
    /// Display name of the genre when the API includes it.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Narrator / duration / part counts nested under a library audiobook.
pub struct AudiobookInfo {
    #[serde(default)]
    /// Narrator display names when present.
    pub narrators: Option<Vec<String>>,
    /// Duration in seconds.
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    /// Chapter/track count advertised by the catalog.
    pub track_count: Option<u32>,
    #[serde(default)]
    /// Download-part count (ZIP pieces or a single M4B).
    pub parts_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Per-user library flags (added date, finished, hidden).
pub struct UserMetadata {
    #[serde(default)]
    /// When the title was added to the user's library (API date string).
    pub added_at: Option<String>,
    #[serde(default)]
    /// True when the user marked the title finished.
    pub finished: Option<bool>,
    #[serde(default)]
    /// True when the user hid the title from the library list.
    pub hidden: Option<bool>,
}

/// Download manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadManifest {
    /// ISBN identifier.
    #[serde(default, deserialize_with = "deserialize_optional_isbn")]
    pub isbn: Option<String>,
    /// Parts.
    #[serde(default)]
    pub parts: Vec<DownloadPart>,
    /// Tracks.
    #[serde(default)]
    pub tracks: Vec<ManifestTrack>,
    /// Expires at.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Size bytes.
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Download part.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadPart {
    /// URL.
    pub url: String,
    /// Size bytes.
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Manifest track.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManifestTrack {
    /// Number.
    #[serde(default)]
    pub number: Option<u32>,
    /// APK `ApiTrack` uses `length_msec` (Gson `@SerializedName`).
    #[serde(default)]
    pub length_msec: Option<u64>,
    /// Legacy / fixture-only key; prefer `length_msec` when both are present.
    #[serde(default)]
    pub length_sec: Option<u64>,
    /// Chapter title.
    #[serde(default)]
    pub chapter_title: Option<String>,
}

impl ManifestTrack {
    /// Duration in milliseconds (APK wire format), falling back to `length_sec`.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.length_msec
            .or_else(|| self.length_sec.map(|s| s.saturating_mul(1000)))
            .unwrap_or(0)
    }
}

/// Packaged m4b.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackagedM4b {
    /// M4b URL.
    pub m4b_url: String,
}

#[derive(Debug, Deserialize)]
/// Error object some Libro.fm endpoints return alongside a 2xx or 4xx body.
struct ApiErrorBody {
    /// Machine/error token when the API uses an `error` field.
    error: Option<String>,
    /// Human-readable message when the API uses `message` instead.
    message: Option<String>,
}

/// Accepts ISBN as a JSON string or number (mobile API is inconsistent).
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

/// Same as [`deserialize_isbn`] but treats missing/null as `None`.
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

/// Accepts authors as a string, string array, or `{name}` objects.
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
