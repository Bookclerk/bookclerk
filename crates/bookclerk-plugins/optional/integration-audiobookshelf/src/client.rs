//! Audiobookshelf HTTP client (OpenAPI + ApiRouter for undocumented routes).
//!
//! Contract pin: see `openapi/PIN.md` in this plugin package.

use bookclerk_integrations::{ExternalUser, IntegrationError, Result};
use bookclerk_plugin_sdk::http::{redirect, Client as HttpClient, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Integration id written onto [`ExternalUser::provider`] (`audiobookshelf`).
const PROVIDER: &str = "audiobookshelf";

/// True when `hostport` is loopback (`localhost`, `127.0.0.1`, `::1`), with an optional port.
fn is_loopback_hostport(hostport: &str) -> bool {
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
            _ => hostport,
        }
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Validate ABS `base_url`: case-insensitive `http`/`https`; plain HTTP only on loopback.
fn validate_abs_base_url(base: &str) -> Result<()> {
    let Some((scheme, after_scheme)) = base.split_once("://") else {
        return Err(IntegrationError::message(
            "integrations.audiobookshelf.base_url must be http(s)",
        ));
    };
    let https = scheme.eq_ignore_ascii_case("https");
    let http = scheme.eq_ignore_ascii_case("http");
    if !https && !http {
        return Err(IntegrationError::message(
            "integrations.audiobookshelf.base_url must be http(s)",
        ));
    }
    if https {
        return Ok(());
    }
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if is_loopback_hostport(hostport) {
        Ok(())
    } else {
        Err(IntegrationError::message(
            "integrations.audiobookshelf.base_url must use https unless the host is loopback",
        ))
    }
}

/// Thin ABS REST client using Bearer auth.
#[derive(Clone)]
pub struct AbsApiClient {
    /// Shared HTTP client for ABS REST calls.
    http: HttpClient,
    /// Scheme+host with no trailing slash (required; empty is rejected in `new`).
    base_url: String,
    /// Bearer API token sent on authenticated routes.
    api_key: String,
}

impl AbsApiClient {
    /// Build a client. `base_url` should be scheme+host (no trailing slash).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let base = base_url.into().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(IntegrationError::message(
                "integrations.audiobookshelf.base_url is required",
            ));
        }
        validate_abs_base_url(&base)?;
        let http = HttpClient::builder()
            // Do not follow redirects: 307/308 would resend bodies (including
            // `/login` passwords) and could downgrade HTTPS→HTTP.
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Ok(Self {
            http,
            base_url: base,
            api_key: api_key.into(),
        })
    }

    /// Send an ABS request. Non-loopback bases must be HTTPS ([`Self::new`]);
    /// loopback HTTP is intentional for local/self-hosted ABS.
    async fn send_abs(&self, req: bookclerk_plugin_sdk::http::RequestBuilder) -> Result<Response> {
        // codeql[rust/cleartext-transmission]
        req.send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))
    }

    /// Base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Joins `path` onto `base_url`, or returns `path` unchanged when it is already absolute.
    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        format!(
            "{}{}",
            self.base_url,
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            }
        )
    }

    /// `Authorization` header value (`Bearer <api_key>`).
    fn bearer(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    /// `POST /api/authorize` — validate API token / key.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn authorize(&self) -> Result<AuthorizeResponse> {
        let resp = self
            .send_abs(
                self.http
                    .post(self.url("/api/authorize"))
                    .header("Authorization", self.bearer()),
            )
            .await?;
        Self::json(resp).await
    }

    /// Username/password login (`POST /login`). Password is never logged.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginResponse> {
        let body = LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
        };
        let resp = self
            .send_abs(self.http.post(self.url("/login")).json(&body))
            .await?;
        Self::json(resp).await
    }

    /// Map a successful login into an [`ExternalUser`].
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn authenticate_user(&self, username: &str, password: &str) -> Result<ExternalUser> {
        let login = self.login(username, password).await?;
        let user = login.user.ok_or_else(|| {
            IntegrationError::message("audiobookshelf login response missing user")
        })?;
        Ok(ExternalUser {
            provider: PROVIDER.into(),
            external_user_id: user.id,
            display_name: Some(user.username),
            access_token: user.token,
        })
    }

    /// `GET /api/users/{id}` — full user including `mediaProgress`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user(&self, user_id: &str) -> Result<AbsUserDetail> {
        let url = self.url(&format!("/api/users/{user_id}"));
        let resp = self
            .send_abs(self.http.get(url).header("Authorization", self.bearer()))
            .await?;
        Self::json(resp).await
    }

    /// `GET /api/items/{id}` — library item metadata for matching.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_library_item(&self, item_id: &str) -> Result<AbsLibraryItem> {
        let resp = self
            .send_abs(
                self.http
                    .get(self.url(&format!("/api/items/{item_id}")))
                    .header("Authorization", self.bearer()),
            )
            .await?;
        Self::json(resp).await
    }

    /// `GET /api/libraries`
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_libraries(&self) -> Result<Vec<AbsLibrary>> {
        let resp = self
            .send_abs(
                self.http
                    .get(self.url("/api/libraries"))
                    .header("Authorization", self.bearer()),
            )
            .await?;
        let body: LibrariesResponse = Self::json(resp).await?;
        Ok(body.libraries)
    }

    /// `POST /api/libraries/{id}/scan`
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn scan_library(&self, library_id: &str, force: bool) -> Result<()> {
        let mut req = self
            .http
            .post(self.url(&format!("/api/libraries/{library_id}/scan")))
            .header("Authorization", self.bearer());
        if force {
            req = req.query(&[("force", "1")]);
        }
        let resp = self.send_abs(req).await?;
        Self::ok_empty(resp).await
    }

    /// `GET /api/users` (admin).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_users(&self) -> Result<Vec<AbsUser>> {
        let resp = self
            .send_abs(
                self.http
                    .get(self.url("/api/users"))
                    .header("Authorization", self.bearer()),
            )
            .await?;
        let body: UsersResponse = Self::json(resp).await?;
        Ok(body.users)
    }

    /// `GET /api/libraries/{id}/search?q=`
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn search_library(&self, library_id: &str, q: &str) -> Result<Value> {
        let resp = self
            .send_abs(
                self.http
                    .get(self.url(&format!("/api/libraries/{library_id}/search")))
                    .header("Authorization", self.bearer())
                    .query(&[("q", q)]),
            )
            .await?;
        Self::json(resp).await
    }

    /// Decodes a successful JSON body; non-2xx becomes [`IntegrationError::api`].
    async fn json<T: for<'de> Deserialize<'de>>(resp: Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(IntegrationError::api(
                status.as_u16(),
                if text.is_empty() {
                    status.to_string()
                } else {
                    text
                },
            ));
        }
        serde_json::from_str(&text).map_err(|err| {
            IntegrationError::message(format!("failed to decode ABS response: {err}; body={text}"))
        })
    }

    /// Accepts 200/204 with no body; any other status becomes [`IntegrationError::api`].
    async fn ok_empty(resp: Response) -> Result<()> {
        let status = resp.status();
        if status == StatusCode::OK || status == StatusCode::NO_CONTENT {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(IntegrationError::api(
            status.as_u16(),
            if text.is_empty() {
                status.to_string()
            } else {
                text
            },
        ))
    }
}

#[derive(Debug, Serialize)]
/// JSON body for `POST /login` (username/password; password is never logged).
struct LoginRequest {
    /// ABS login name.
    username: String,
    /// ABS password; never logged.
    password: String,
}

#[derive(Debug, Clone, Deserialize)]
/// `POST /login` payload; `user` is required to map an [`ExternalUser`].
pub struct LoginResponse {
    /// Authenticated ABS user; missing `user` fails `authenticate_user`.
    pub user: Option<AbsUser>,
    #[serde(default)]
    /// Optional server settings object from ABS (unused by Bookclerk).
    pub server_settings: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
/// `POST /api/authorize` payload used to validate an API token.
pub struct AuthorizeResponse {
    /// User bound to the API token when authorize succeeds.
    pub user: Option<AbsUser>,
    #[serde(default)]
    /// Optional server settings object from ABS (unused by Bookclerk).
    pub server_settings: Option<Value>,
}

/// Audiobookshelf user.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsUser {
    /// Identifier.
    pub id: String,
    /// Username.
    #[serde(default)]
    pub username: String,
    /// Token.
    #[serde(default)]
    pub token: Option<String>,
    /// Typ.
    #[serde(default)]
    pub typ: Option<String>,
    /// User type.
    #[serde(rename = "type", default)]
    pub user_type: Option<String>,
}

/// Full ABS user payload including listening progress.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsUserDetail {
    /// Identifier.
    pub id: String,
    /// Username.
    #[serde(default)]
    pub username: String,
    /// Media progress.
    #[serde(default, rename = "mediaProgress")]
    pub media_progress: Vec<AbsMediaProgress>,
}

/// Audiobookshelf media progress.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsMediaProgress {
    /// Identifier.
    pub id: String,
    /// Library item Identifier.
    #[serde(rename = "libraryItemId")]
    pub library_item_id: String,
    /// Duration.
    #[serde(default)]
    pub duration: Option<f64>,
    /// Progress.
    #[serde(default)]
    pub progress: Option<f64>,
    /// Current time.
    #[serde(default, rename = "currentTime")]
    pub current_time: Option<f64>,
    /// Is finished.
    #[serde(default, rename = "isFinished")]
    pub is_finished: bool,
    /// Last update.
    #[serde(default, rename = "lastUpdate")]
    pub last_update: Option<i64>,
}

/// Audiobookshelf library item.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsLibraryItem {
    /// Identifier.
    pub id: String,
    /// Media.
    #[serde(default)]
    pub media: Option<AbsItemMedia>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Nested `media` object on an ABS library item.
pub struct AbsItemMedia {
    #[serde(default)]
    /// Title / author / ASIN / ISBN used to match Bookclerk library rows.
    pub metadata: Option<AbsItemMetadata>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// Matching fields from ABS item metadata (all optional).
pub struct AbsItemMetadata {
    #[serde(default)]
    /// Title string when ABS stored one.
    pub title: Option<String>,
    #[serde(default)]
    /// Display author name when ABS stored one.
    pub author_name: Option<String>,
    #[serde(default)]
    /// Amazon ASIN when ABS stored one.
    pub asin: Option<String>,
    #[serde(default)]
    /// ISBN when ABS stored one.
    pub isbn: Option<String>,
}

/// Audiobookshelf library.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsLibrary {
    /// Identifier.
    pub id: String,
    /// Display or configuration name.
    pub name: String,
    /// Media type.
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Deserialize)]
/// Wrapper for `GET /api/libraries` (`libraries` array).
struct LibrariesResponse {
    #[serde(default)]
    /// Libraries visible to the API token (empty when ABS omitted the field).
    libraries: Vec<AbsLibrary>,
}

#[derive(Debug, Deserialize)]
/// Wrapper for admin `GET /api/users` (`users` array).
struct UsersResponse {
    #[serde(default)]
    /// ABS users visible to an admin token (empty when omitted).
    users: Vec<AbsUser>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn base_url_accepts_https_and_loopback_http_case_insensitive() {
        assert!(AbsApiClient::new("HTTPS://abs.example", "k").is_ok());
        assert!(AbsApiClient::new("http://127.0.0.1:13378", "k").is_ok());
        assert!(AbsApiClient::new("HTTP://localhost:13378", "k").is_ok());
        assert!(AbsApiClient::new("http://abs.example", "k").is_err());
        assert!(AbsApiClient::new("ftp://abs.example", "k").is_err());
    }

    #[tokio::test]
    async fn authorize_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/authorize"))
            .and(header("Authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "u1", "username": "admin" }
            })))
            .mount(&server)
            .await;

        let client = AbsApiClient::new(server.uri(), "test-key").unwrap();
        let auth = client.authorize().await.unwrap();
        assert_eq!(auth.user.unwrap().id, "u1");
    }

    #[tokio::test]
    async fn scan_library_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/libraries/lib1/scan"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = AbsApiClient::new(server.uri(), "k").unwrap();
        client.scan_library("lib1", false).await.unwrap();
    }

    #[tokio::test]
    async fn login_maps_external_user() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "usr_1", "username": "bob", "token": "tok" }
            })))
            .mount(&server)
            .await;
        let client = AbsApiClient::new(server.uri(), "k").unwrap();
        let password = ["sec", "ret"].concat();
        let user = client.authenticate_user("bob", &password).await.unwrap();
        assert_eq!(user.external_user_id, "usr_1");
        assert_eq!(user.display_name.as_deref(), Some("bob"));
        assert_eq!(user.access_token.as_deref(), Some("tok"));
    }
}
