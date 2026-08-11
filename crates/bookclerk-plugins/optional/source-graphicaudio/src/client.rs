//! HTTP client for the GraphicAudio Android Retrofit API.

use std::path::Path;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};

use crate::error::{GraphicAudioError, Result};

/// Default API origin (no trailing slash after `/access`).
pub const DEFAULT_BASE_URL: &str = "https://www.graphicaudio.net/access";

/// Device activation / password login.
pub const LOGIN_PATH: &str = "/activation/login";

/// Forget a device activation (`client_id` form field).
pub const REMOVE_PATH: &str = "/activation/remove";

/// Library + sample products listing.
pub const PRODUCTS_PATH: &str = "/api/products";

/// Download URL lookup (`?product=`).
pub const LINKS_PATH: &str = "/api/links";

const USER_AGENT_VALUE: &str = "okhttp/4.12.0 GraphicAudio/Bookclerk";

/// Authenticated GraphicAudio HTTP helper.
#[derive(Debug, Clone)]
pub struct GraphicAudioClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl Default for GraphicAudioClient {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_URL)
    }
}

impl GraphicAudioClient {
    /// Build a client pointed at `base_url` (trailing slash stripped).
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: reqwest::Client::new(),
            base_url,
            token: None,
        }
    }

    /// Override the HTTP client (tests / custom timeouts).
    #[must_use]
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Attach an activation token for subsequent requests.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Current token, if any.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Base URL in use.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn headers(&self, with_auth: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        if with_auth {
            let token = self
                .token
                .as_deref()
                .ok_or_else(|| GraphicAudioError::auth("not logged in"))?;
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(token)
                    .map_err(|e| GraphicAudioError::auth(format!("invalid token header: {e}")))?,
            );
        }
        Ok(headers)
    }

    /// `POST activation/login` — returns the opaque token string.
    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
        client_id: &str,
    ) -> Result<String> {
        let url = format!("{}{LOGIN_PATH}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.headers(false)?)
            .form(&[
                ("username", username),
                ("password", password),
                ("client_id", client_id),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<LoginErrorBody>(&body)
                .ok()
                .and_then(|e| e.message.or(e.title))
                .unwrap_or(body);
            return Err(GraphicAudioError::auth(format!(
                "login failed ({status}): {msg}"
            )));
        }
        let parsed: LoginOk = serde_json::from_str(&body)?;
        let token = parsed
            .token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| GraphicAudioError::auth("login response missing token"))?;
        self.token = Some(token.clone());
        Ok(token)
    }

    /// `GET api/products` — owned titles plus promotional samples.
    pub async fn products(&self) -> Result<Vec<Product>> {
        let url = format!("{}{PRODUCTS_PATH}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .headers(self.headers(true)?)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(GraphicAudioError::api(format!(
                "products failed ({status}): {body}"
            )));
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// `GET api/links?product=` — Lo/Hi plain media URLs.
    pub async fn links(&self, product_id: &str) -> Result<DownloadLinks> {
        let url = format!("{}{LINKS_PATH}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("product", product_id)])
            .headers(self.headers(true)?)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(GraphicAudioError::api(format!(
                "links failed ({status}): {body}"
            )));
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Download bytes from an absolute media URL (no Authorization).
    ///
    /// Prefer [`Self::download_to_path`] for large Hi/Lo media (~100MB–500MB+).
    pub async fn download_bytes(&self, url: &str) -> Result<bytes::Bytes> {
        let resp = self
            .http
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(GraphicAudioError::download(format!(
                "download failed ({status}) for {url}"
            )));
        }
        Ok(resp.bytes().await?)
    }

    /// Stream an absolute media URL to `path` without buffering the whole body.
    pub async fn download_to_path(&self, url: &str, path: &Path) -> Result<()> {
        let resp = self
            .http
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        crate::http_util::response_to_path(resp, path).await
    }

    /// `POST activation/remove` — drop a device slot for `client_id`.
    pub async fn remove_activation(&self, client_id: &str) -> Result<()> {
        let url = format!("{}{REMOVE_PATH}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.headers(true)?)
            .form(&[("client_id", client_id)])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<LoginErrorBody>(&body)
                .ok()
                .and_then(|e| e.message.or(e.title))
                .unwrap_or(body);
            return Err(GraphicAudioError::auth(format!(
                "activation remove failed ({status}): {msg}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct LoginOk {
    #[serde(alias = "Token")]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginErrorBody {
    #[serde(alias = "Message")]
    message: Option<String>,
    #[serde(alias = "Title")]
    title: Option<String>,
}

/// One library / sample product from `api/products`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    /// Identifier.
    #[serde(rename = "Id")]
    pub id: String,
    /// Product type.
    #[serde(rename = "Type", default)]
    pub product_type: Option<String>,
    /// Product name.
    #[serde(rename = "ProductName", default)]
    pub product_name: Option<String>,
    /// Title.
    #[serde(rename = "Title", default)]
    pub title: Option<String>,
    /// Author.
    #[serde(rename = "Author", default)]
    pub author: Option<String>,
    /// Series.
    #[serde(rename = "Series", default)]
    pub series: Option<String>,
    /// Episode.
    #[serde(rename = "Episode", default)]
    pub episode: Option<String>,
    /// Genre.
    #[serde(rename = "Genre", default)]
    pub genre: Option<String>,
    /// Image.
    #[serde(rename = "Image", default)]
    pub image: Option<String>,
    /// Thumbnail.
    #[serde(rename = "Thumbnail", default)]
    pub thumbnail: Option<String>,
    /// Running time.
    #[serde(rename = "Running Time", default)]
    pub running_time: Option<String>,
    #[serde(
        rename = "Purchased Date",
        default,
        deserialize_with = "deserialize_opt_stringish"
    )]
    /// Purchased date.
    pub purchased_date: Option<String>,
    #[serde(
        rename = "Release Date",
        default,
        deserialize_with = "deserialize_opt_stringish"
    )]
    /// Release date.
    pub release_date: Option<String>,
}

impl Product {
    /// Whether this entry is a promotional sample (not owned).
    #[must_use]
    pub fn is_sample(&self) -> bool {
        self.product_type
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case("sample"))
            .unwrap_or(false)
    }

    /// Display title preferring `Series` + `ProductName` for owned volumes.
    ///
    /// Access App owned rows often use a short `ProductName` (e.g. `"Volume 1"`)
    /// with the work title in `Series`. Magento downloadable rows use the full
    /// storefront title — combining these fields keeps ZIP matching reliable.
    #[must_use]
    pub fn display_title(&self) -> String {
        let series = self
            .series
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let name = self
            .product_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (series, name) {
            (Some(series), Some(name)) => {
                if name
                    .to_ascii_lowercase()
                    .contains(&series.to_ascii_lowercase())
                    || series
                        .to_ascii_lowercase()
                        .contains(&name.to_ascii_lowercase())
                {
                    // Prefer the longer / more specific label.
                    if name.len() >= series.len() {
                        name.to_string()
                    } else {
                        series.to_string()
                    }
                } else {
                    format!("{series} {name}")
                }
            }
            (Some(series), None) => series.to_string(),
            (None, Some(name)) => name.to_string(),
            (None, None) => self
                .title
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(self.id.as_str())
                .to_string(),
        }
    }
}

/// Lo/Hi download URLs from `api/links`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadLinks {
    /// Lo.
    #[serde(rename = "Lo", default)]
    pub lo: Option<String>,
    /// Hi.
    #[serde(rename = "Hi", default)]
    pub hi: Option<String>,
}

impl DownloadLinks {
    /// Prefer high-quality URL, else low.
    #[must_use]
    pub fn preferred_url(&self) -> Option<&str> {
        self.url_for_quality(true)
    }

    /// Select Hi or Lo based on whether high quality is preferred.
    #[must_use]
    pub fn url_for_quality(&self, prefer_hi: bool) -> Option<&str> {
        let hi = self.hi.as_deref().filter(|u| !u.is_empty());
        let lo = self.lo.as_deref().filter(|u| !u.is_empty());
        if prefer_hi {
            hi.or(lo)
        } else {
            lo.or(hi)
        }
    }
}

/// Accept JSON string, number, or null as `Option<String>` (Access App dates are epoch ints).
fn deserialize_opt_stringish<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(other) => Some(other.to_string()),
    })
}

#[cfg(test)]
mod title_tests {
    use super::*;

    #[test]
    fn display_title_joins_series_and_volume() {
        let p = Product {
            id: "5273".into(),
            product_type: Some("owned".into()),
            product_name: Some("Volume 1".into()),
            title: Some("MP3 (256kbps)".into()),
            author: Some("Pierce Brown".into()),
            series: Some("Red Rising: Sons of Ares".into()),
            episode: None,
            genre: None,
            image: None,
            thumbnail: None,
            running_time: None,
            purchased_date: None,
            release_date: None,
        };
        assert_eq!(p.display_title(), "Red Rising: Sons of Ares Volume 1");
    }
}
