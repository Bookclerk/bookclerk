//! Audiobookshelf HTTP client (OpenAPI + ApiRouter for undocumented routes).
//!
//! Contract pin: see `openapi/PIN.md` in this plugin package.

use bookclerk_integrations::{ExternalUser, IntegrationError, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PROVIDER: &str = "audiobookshelf";

/// Thin ABS REST client using Bearer auth.
#[derive(Clone)]
pub struct AbsApiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl AbsApiClient {
    /// Build a client. `base_url` should be scheme+host (no trailing slash).
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let base = base_url.into().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(IntegrationError::message(
                "integrations.audiobookshelf.base_url is required",
            ));
        }
        Ok(Self {
            http: reqwest::Client::new(),
            base_url: base,
            api_key: api_key.into(),
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

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

    fn bearer(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    /// `POST /api/authorize` — validate API token / key.
    pub async fn authorize(&self) -> Result<AuthorizeResponse> {
        let resp = self
            .http
            .post(self.url("/api/authorize"))
            .header("Authorization", self.bearer())
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Self::json(resp).await
    }

    /// Username/password login (`POST /login`). Password is never logged.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginResponse> {
        let body = LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
        };
        let resp = self
            .http
            .post(self.url("/login"))
            .json(&body)
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Self::json(resp).await
    }

    /// Map a successful login into an [`ExternalUser`].
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
    pub async fn get_user(&self, user_id: &str) -> Result<AbsUserDetail> {
        let resp = self
            .http
            .get(self.url(&format!("/api/users/{user_id}")))
            .header("Authorization", self.bearer())
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Self::json(resp).await
    }

    /// `GET /api/items/{id}` — library item metadata for matching.
    pub async fn get_library_item(&self, item_id: &str) -> Result<AbsLibraryItem> {
        let resp = self
            .http
            .get(self.url(&format!("/api/items/{item_id}")))
            .header("Authorization", self.bearer())
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Self::json(resp).await
    }

    /// `GET /api/libraries`
    pub async fn list_libraries(&self) -> Result<Vec<AbsLibrary>> {
        let resp = self
            .http
            .get(self.url("/api/libraries"))
            .header("Authorization", self.bearer())
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        let body: LibrariesResponse = Self::json(resp).await?;
        Ok(body.libraries)
    }

    /// `POST /api/libraries/{id}/scan`
    pub async fn scan_library(&self, library_id: &str, force: bool) -> Result<()> {
        let mut req = self
            .http
            .post(self.url(&format!("/api/libraries/{library_id}/scan")))
            .header("Authorization", self.bearer());
        if force {
            req = req.query(&[("force", "1")]);
        }
        let resp = req
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Self::ok_empty(resp).await
    }

    /// `GET /api/users` (admin).
    pub async fn list_users(&self) -> Result<Vec<AbsUser>> {
        let resp = self
            .http
            .get(self.url("/api/users"))
            .header("Authorization", self.bearer())
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        let body: UsersResponse = Self::json(resp).await?;
        Ok(body.users)
    }

    /// `GET /api/libraries/{id}/search?q=`
    pub async fn search_library(&self, library_id: &str, q: &str) -> Result<Value> {
        let resp = self
            .http
            .get(self.url(&format!("/api/libraries/{library_id}/search")))
            .header("Authorization", self.bearer())
            .query(&[("q", q)])
            .send()
            .await
            .map_err(|err| IntegrationError::message(err.to_string()))?;
        Self::json(resp).await
    }

    async fn json<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
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

    async fn ok_empty(resp: reqwest::Response) -> Result<()> {
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
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub user: Option<AbsUser>,
    #[serde(default)]
    pub server_settings: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeResponse {
    pub user: Option<AbsUser>,
    #[serde(default)]
    pub server_settings: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsUser {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub typ: Option<String>,
    #[serde(rename = "type", default)]
    pub user_type: Option<String>,
}

/// Full ABS user payload including listening progress.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsUserDetail {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default, rename = "mediaProgress")]
    pub media_progress: Vec<AbsMediaProgress>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsMediaProgress {
    pub id: String,
    #[serde(rename = "libraryItemId")]
    pub library_item_id: String,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default, rename = "currentTime")]
    pub current_time: Option<f64>,
    #[serde(default, rename = "isFinished")]
    pub is_finished: bool,
    #[serde(default, rename = "lastUpdate")]
    pub last_update: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsLibraryItem {
    pub id: String,
    #[serde(default)]
    pub media: Option<AbsItemMedia>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsItemMedia {
    #[serde(default)]
    pub metadata: Option<AbsItemMetadata>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsItemMetadata {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbsLibrary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LibrariesResponse {
    #[serde(default)]
    libraries: Vec<AbsLibrary>,
}

#[derive(Debug, Deserialize)]
struct UsersResponse {
    #[serde(default)]
    users: Vec<AbsUser>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
        let user = client.authenticate_user("bob", "secret").await.unwrap();
        assert_eq!(user.external_user_id, "usr_1");
        assert_eq!(user.display_name.as_deref(), Some("bob"));
        assert_eq!(user.access_token.as_deref(), Some("tok"));
    }
}
