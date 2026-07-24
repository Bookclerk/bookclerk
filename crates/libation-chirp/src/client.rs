//! GraphQL client for Chirp's Mockingjay Android API.

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ChirpError, Result};

/// Android Mockingjay GraphQL endpoint.
pub const DEFAULT_GRAPHQL_URL: &str = "https://api.chirpbooks.com/api/graphql";

const USER_AGENT_VALUE: &str = "okhttp/4.12.0 Chirp/Libation";

const SIGN_IN: &str = r#"
mutation signIn($email: String!, $password: String!) {
  signIn(email: $email, password: $password) {
    user { id token webToken email }
  }
}
"#;

const LIBRARY_PAGE: &str = r#"
query AndroidCurrentUserAudiobooks($page: Int!, $pageSize: Int!) {
  currentUserAudiobooks(
    page: $page
    pageSize: $pageSize
    sort: TITLE_A_Z
    clientCapabilities: [CHIRP_AUDIO]
  ) {
    id
    archived
    playable
    audiobook {
      id
      displayTitle
      subTitle
      displayAuthors
      displayNarrators
      durationMs
      abridged
      publisher
      releasedOn
      coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 700)
      description
    }
  }
}
"#;

const SINGLE_AUDIOBOOK: &str = r#"
query AndroidSingleAudiobook($id: ID!) {
  audiobook(id: $id, clientCapabilities: [CHIRP_AUDIO]) {
    id
    displayTitle
    coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 700)
    tracks {
      id
      mediaUrl
      chapterNumber
      partNumber
      durationMs
      offsetFromBookStartMs
      displayName
    }
    displayAuthors
    displayNarrators
    durationMs
    abridged
    publisher
    releasedOn
    subTitle
  }
}
"#;

/// Authenticated Chirp GraphQL helper.
#[derive(Debug, Clone)]
pub struct ChirpClient {
    http: Client,
    graphql_url: String,
    access_token: Option<String>,
}

impl Default for ChirpClient {
    fn default() -> Self {
        Self::new(DEFAULT_GRAPHQL_URL)
    }
}

impl ChirpClient {
    #[must_use]
    pub fn new(graphql_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            graphql_url: graphql_url.into(),
            access_token: None,
        }
    }

    #[must_use]
    pub fn with_http(mut self, http: Client) -> Self {
        self.http = http;
        self
    }

    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    #[must_use]
    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    #[must_use]
    pub fn graphql_url(&self) -> &str {
        &self.graphql_url
    }

    fn headers(&self, with_auth: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        if with_auth {
            let token = self
                .access_token
                .as_deref()
                .ok_or_else(|| ChirpError::auth("not logged in"))?;
            let value = format!("Bearer {token}");
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&value)
                    .map_err(|e| ChirpError::auth(format!("invalid token header: {e}")))?,
            );
        }
        Ok(headers)
    }

    async fn graphql(
        &self,
        operation_name: &str,
        query: &str,
        variables: Value,
        with_auth: bool,
    ) -> Result<Value> {
        let body = json!({
            "operationName": operation_name,
            "query": query,
            "variables": variables,
        });
        let resp = self
            .http
            .post(&self.graphql_url)
            .headers(self.headers(with_auth)?)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ChirpError::api(format!(
                "GraphQL HTTP {status}: {}",
                truncate(&text, 300)
            )));
        }
        let parsed: Value = serde_json::from_str(&text)?;
        if let Some(errors) = parsed.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                let msg = errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .collect::<Vec<_>>()
                    .join("; ");
                // Some operations return data:null with errors (e.g. bad password).
                if parsed
                    .get("data")
                    .and_then(|d| d.as_object())
                    .is_none_or(|o| o.values().all(|v| v.is_null()))
                {
                    return Err(ChirpError::api(msg));
                }
                tracing::debug!(%msg, "GraphQL returned errors with partial data");
            }
        }
        Ok(parsed)
    }

    /// GraphQL `signIn` mutation.
    pub async fn login(&mut self, email: &str, password: &str) -> Result<SignInUser> {
        let parsed = self
            .graphql(
                "signIn",
                SIGN_IN,
                json!({ "email": email, "password": password }),
                false,
            )
            .await
            .map_err(|e| match e {
                ChirpError::Api(m) if m.to_ascii_lowercase().contains("invalid") => {
                    ChirpError::auth(m)
                }
                other => other,
            })?;
        let user = parsed
            .pointer("/data/signIn/user")
            .cloned()
            .ok_or_else(|| ChirpError::auth("signIn response missing user"))?;
        let user: SignInUser = serde_json::from_value(user)?;
        if user.token.is_empty() {
            return Err(ChirpError::auth("signIn response missing token"));
        }
        self.access_token = Some(user.token.clone());
        Ok(user)
    }

    /// Paginated owned library (`currentUserAudiobooks`).
    pub async fn library_page(&self, page: u32, page_size: u32) -> Result<Vec<LibraryItem>> {
        let parsed = self
            .graphql(
                "AndroidCurrentUserAudiobooks",
                LIBRARY_PAGE,
                json!({ "page": page, "pageSize": page_size }),
                true,
            )
            .await?;
        let items = parsed
            .pointer("/data/currentUserAudiobooks")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        Ok(serde_json::from_value(items)?)
    }

    /// Full audiobook with track `mediaUrl`s.
    pub async fn audiobook(&self, id: &str) -> Result<Audiobook> {
        let parsed = self
            .graphql(
                "AndroidSingleAudiobook",
                SINGLE_AUDIOBOOK,
                json!({ "id": id }),
                true,
            )
            .await?;
        let book = parsed
            .pointer("/data/audiobook")
            .cloned()
            .ok_or_else(|| ChirpError::api(format!("audiobook {id} not found")))?;
        if book.is_null() {
            return Err(ChirpError::api(format!("audiobook {id} not found")));
        }
        Ok(serde_json::from_value(book)?)
    }

    /// Download bytes from an absolute media URL.
    pub async fn download_bytes(&self, url: &str) -> Result<bytes::Bytes> {
        let resp = self
            .http
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ChirpError::download(format!(
                "download failed ({status}) for {url}"
            )));
        }
        Ok(resp.bytes().await?)
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

/// User object from `signIn`.
#[derive(Debug, Clone, Deserialize)]
pub struct SignInUser {
    pub id: String,
    pub token: String,
    #[serde(default, rename = "webToken")]
    pub web_token: Option<String>,
    pub email: String,
}

/// One `currentUserAudiobooks` row.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryItem {
    pub id: String,
    #[serde(default)]
    pub archived: Option<bool>,
    #[serde(default)]
    pub playable: Option<bool>,
    pub audiobook: Option<Audiobook>,
}

/// Chirp audiobook metadata (+ optional tracks).
#[derive(Debug, Clone, Deserialize)]
pub struct Audiobook {
    pub id: String,
    #[serde(default, rename = "displayTitle")]
    pub display_title: Option<String>,
    #[serde(default, rename = "subTitle")]
    pub sub_title: Option<String>,
    #[serde(default, rename = "displayAuthors")]
    pub display_authors: Option<String>,
    #[serde(default, rename = "displayNarrators")]
    pub display_narrators: Option<String>,
    #[serde(default, rename = "durationMs")]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub abridged: Option<bool>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default, rename = "releasedOn")]
    pub released_on: Option<String>,
    #[serde(default, rename = "coverUrl")]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

/// One downloadable / playable track.
#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: String,
    #[serde(default, rename = "mediaUrl")]
    pub media_url: Option<String>,
    #[serde(default, rename = "chapterNumber")]
    pub chapter_number: Option<i64>,
    #[serde(default, rename = "partNumber")]
    pub part_number: Option<i64>,
    #[serde(default, rename = "durationMs")]
    pub duration_ms: Option<u64>,
    #[serde(default, rename = "offsetFromBookStartMs")]
    pub offset_from_book_start_ms: Option<u64>,
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
}
