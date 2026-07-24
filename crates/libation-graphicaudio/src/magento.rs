//! Magento storefront access: customer login, downloadable ZIPs, Browser Player.

use std::io::copy;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::cookie::Jar;
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode, Url};

use crate::error::{GraphicAudioError, Result};

/// Default Magento storefront origin (no `/access` suffix).
pub const DEFAULT_STORE_URL: &str = "https://www.graphicaudio.net";

const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Libation/GraphicAudio";

/// One row from My Downloadable Products.
#[derive(Debug, Clone)]
pub struct DownloadableProduct {
    pub title: String,
    pub option_label: String,
    pub download_url: String,
    pub remaining: Option<u32>,
    pub status: String,
}

/// One owned title from Browser Player `content_library` HTML.
#[derive(Debug, Clone)]
pub struct LibraryItem {
    pub product_id: String,
    pub title: Option<String>,
    pub listen_path: Option<String>,
}

impl DownloadableProduct {
    /// Prefer M4B ZIP, then MP3, then FLAC, then anything else.
    #[must_use]
    pub fn format_rank(&self) -> u8 {
        let label = self.option_label.to_ascii_lowercase();
        if label.contains("m4b") {
            0
        } else if label.contains("mp3") {
            1
        } else if label.contains("flac") {
            2
        } else {
            3
        }
    }

    #[must_use]
    pub fn has_remaining(&self) -> bool {
        match self.remaining {
            None => true,
            Some(n) => n > 0,
        }
    }
}

/// Magento customer session + cookie jar (including Browser Player CloudFront cookies).
#[derive(Debug, Clone)]
pub struct MagentoClient {
    http: Client,
    http_no_redirect: Client,
    base_url: String,
}

impl MagentoClient {
    /// Build a client for `base_url` (trailing slash stripped).
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let jar = Arc::new(Jar::default());
        let http = Client::builder()
            .cookie_provider(jar.clone())
            .redirect(Policy::limited(10))
            .user_agent(BROWSER_UA)
            .build()?;
        let http_no_redirect = Client::builder()
            .cookie_provider(jar)
            .redirect(Policy::none())
            .user_agent(BROWSER_UA)
            .build()?;
        Ok(Self {
            http,
            http_no_redirect,
            base_url,
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn abs_url(&self, path_or_url: &str) -> Result<Url> {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            Url::parse(path_or_url).map_err(|e| GraphicAudioError::api(format!("bad url: {e}")))
        } else {
            let base = format!("{}/", self.base_url);
            Url::parse(&base)
                .map_err(|e| GraphicAudioError::api(format!("bad base url: {e}")))?
                .join(path_or_url.trim_start_matches('/'))
                .map_err(|e| GraphicAudioError::api(format!("bad relative url: {e}")))
        }
    }

    /// Customer login via Magento `loginPost` (form_key + email/password).
    pub async fn login(&self, email: &str, password: &str) -> Result<()> {
        let login_url = format!("{}/customer/account/login/", self.base_url);
        let page = self.http.get(&login_url).send().await?;
        let status = page.status();
        let html = page.text().await?;
        if !status.is_success() {
            return Err(GraphicAudioError::auth(format!(
                "Magento login page failed ({status})"
            )));
        }
        let form_key = extract_form_key(&html)
            .ok_or_else(|| GraphicAudioError::auth("Magento login page missing form_key"))?;

        let post_url = format!("{}/customer/account/loginPost/", self.base_url);
        let resp = self
            .http
            .post(&post_url)
            .header(reqwest::header::REFERER, &login_url)
            .form(&[
                ("form_key", form_key.as_str()),
                ("login[username]", email),
                ("login[password]", password),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !(status.is_success() || status.is_redirection()) {
            return Err(GraphicAudioError::auth(format!(
                "Magento loginPost failed ({status})"
            )));
        }
        // Confirm session: account page should not bounce to login.
        let account = self
            .http
            .get(format!("{}/customer/account/", self.base_url))
            .send()
            .await?;
        let account_url = account.url().clone();
        let account_html = account.text().await?;
        if account_url.path().contains("/customer/account/login")
            || account_html.contains("customer/account/login")
                && !account_html.contains("customer/account/logout")
        {
            return Err(GraphicAudioError::auth(
                "Magento login failed (still on login page — check email/password)",
            ));
        }
        let _ = body;
        Ok(())
    }

    /// Parse My Downloadable Products into link rows.
    pub async fn list_downloadable(&self) -> Result<Vec<DownloadableProduct>> {
        let url = format!("{}/downloadable/customer/products/", self.base_url);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let html = resp.text().await?;
        if !status.is_success() {
            return Err(GraphicAudioError::api(format!(
                "downloadable products failed ({status})"
            )));
        }
        if html.contains("/customer/account/login")
            && !html
                .to_ascii_lowercase()
                .contains("my downloadable products")
        {
            return Err(GraphicAudioError::auth(
                "Magento session expired; re-login required for ZIP downloads",
            ));
        }
        Ok(parse_downloadable_products(&html))
    }

    /// Resolve a Magento downloadable link to a CDN URL (or stream body if no redirect).
    ///
    /// Hitting the Magento `/downloadable/download/link/...` URL consumes one of the
    /// limited download attempts — callers must finish the transfer.
    pub async fn resolve_download_url(&self, download_url: &str) -> Result<String> {
        let url = self.abs_url(download_url)?;
        let resp = self.http_no_redirect.get(url).send().await?;
        let status = resp.status();
        if matches!(
            status,
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            let loc = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    GraphicAudioError::download("downloadable link redirect missing Location")
                })?;
            return Ok(self.abs_url(loc)?.to_string());
        }
        if status.is_success() {
            // Rare: Magento streams the ZIP itself. Return the original URL for a
            // follow-up cookie-authenticated GET (attempts already consumed).
            return Ok(download_url.to_string());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(GraphicAudioError::download(format!(
            "downloadable link failed ({status}): {}",
            truncate(&body, 180)
        )))
    }

    /// Stream `url` to `path` (uses Magento/CloudFront cookies when present).
    pub async fn download_to_path(&self, url: &str, path: &Path) -> Result<()> {
        let abs = self.abs_url(url)?;
        let resp = self.http.get(abs).send().await?;
        crate::http_util::response_to_path(resp, path).await
    }

    /// Fetch Browser Player library HTML (`library/index/content_library`).
    pub async fn content_library_html(&self) -> Result<String> {
        let url = format!("{}/library/index/content_library", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .send()
            .await?;
        let status = resp.status();
        let html = resp.text().await?;
        if !status.is_success() {
            return Err(GraphicAudioError::api(format!(
                "content_library failed ({status})"
            )));
        }
        Ok(html)
    }

    /// List owned titles from Browser Player library (no Access App device token).
    pub async fn list_library(&self) -> Result<Vec<LibraryItem>> {
        let html = self.content_library_html().await?;
        Ok(parse_library_items(&html))
    }

    /// Find Browser Player listen URL for `product_id` via `library/index/content_library`.
    pub async fn player_listen_url(&self, product_id: &str) -> Result<String> {
        let html = self.content_library_html().await?;
        find_player_listen_url(&html, product_id).ok_or_else(|| {
            GraphicAudioError::download(format!(
                "no Browser Player listen link for product {product_id}"
            ))
        })
    }

    /// Open the Browser Player page and return the `<audio>` media URL (sets CF cookies).
    pub async fn player_audio_url(&self, product_id: &str) -> Result<String> {
        let listen = self.player_listen_url(product_id).await?;
        let abs = self.abs_url(&listen)?;
        let resp = self.http.get(abs).send().await?;
        let status = resp.status();
        let html = resp.text().await?;
        if !status.is_success() {
            return Err(GraphicAudioError::api(format!(
                "Browser Player page failed ({status})"
            )));
        }
        extract_audio_src(&html).ok_or_else(|| {
            GraphicAudioError::download(format!(
                "Browser Player page for {product_id} has no audio src"
            ))
        })
    }
}

/// Pick the best downloadable ZIP row whose title matches `product_title`.
#[must_use]
pub fn select_downloadable<'a>(
    rows: &'a [DownloadableProduct],
    product_title: &str,
) -> Option<&'a DownloadableProduct> {
    let mut matched: Vec<&DownloadableProduct> = rows
        .iter()
        .filter(|r| r.has_remaining() && titles_match(&r.title, product_title))
        .collect();
    matched.sort_by_key(|r| r.format_rank());
    matched.into_iter().next()
}

/// Download Magento ZIP for `product_title`, extract audio into `title_dir`.
pub async fn fetch_zip_for_title(
    client: &MagentoClient,
    product_title: &str,
    title_dir: &Path,
) -> Result<PathBuf> {
    let rows = client.list_downloadable().await?;
    let row = select_downloadable(&rows, product_title).ok_or_else(|| {
        GraphicAudioError::download(format!(
            "no Magento ZIP download with remaining attempts for `{product_title}`"
        ))
    })?;
    tracing::info!(
        title = %row.title,
        option = %row.option_label,
        remaining = ?row.remaining,
        "GraphicAudio Magento ZIP selected"
    );

    std::fs::create_dir_all(title_dir)?;
    let zip_path = title_dir.join("download.zip");
    // Consume one Magento download attempt: resolve redirect, then stream CDN.
    let cdn_url = client.resolve_download_url(&row.download_url).await?;
    client.download_to_path(&cdn_url, &zip_path).await?;

    let audio = extract_zip_audio(&zip_path, title_dir)?;
    let _ = std::fs::remove_file(&zip_path);
    Ok(audio)
}

/// Browser Player: Magento session → listen page → CF-cookie media download.
pub async fn fetch_browser_audio(
    client: &MagentoClient,
    product_id: &str,
    title_dir: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(title_dir)?;
    let media_url = client.player_audio_url(product_id).await?;
    let ext = extension_from_url(&media_url);
    let path = title_dir.join(format!("browser{ext}"));
    client.download_to_path(&media_url, &path).await?;
    Ok(path)
}

fn extract_zip_audio(zip_path: &Path, title_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| GraphicAudioError::download(format!("zip open: {e}")))?;
    let mut audio_paths = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| GraphicAudioError::download(format!("zip entry: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .enclosed_name()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(format!("entry-{i}")));
        let file_name = name
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("track.bin");
        if !is_audio_filename(file_name) {
            continue;
        }
        let dest = title_dir.join(sanitize_filename(file_name));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        copy(&mut entry, &mut out)?;
        audio_paths.push(dest);
    }
    if audio_paths.is_empty() {
        return Err(GraphicAudioError::download(
            "Magento ZIP contained no audio files",
        ));
    }
    // Prefer a single M4B when present.
    if let Some(m4b) = audio_paths.iter().find(|p| {
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("m4b"))
    }) {
        return Ok(m4b.clone());
    }
    audio_paths.sort();
    Ok(audio_paths.remove(0))
}

fn parse_html_fragment(html: &str) -> scraper::Html {
    // Bare `<tr>` fragments are dropped by html5ever unless wrapped in a table.
    let lower = html.to_ascii_lowercase();
    if lower.contains("<tr") && !lower.contains("<table") {
        scraper::Html::parse_fragment(&format!("<table>{html}</table>"))
    } else {
        scraper::Html::parse_fragment(html)
    }
}

fn extract_form_key(html: &str) -> Option<String> {
    let document = parse_html_fragment(html);
    let selector = scraper::Selector::parse(r#"input[name="form_key"]"#).ok()?;
    document
        .select(&selector)
        .filter_map(|el| el.value().attr("value"))
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(str::to_string)
}

fn parse_downloadable_products(html: &str) -> Vec<DownloadableProduct> {
    let document = parse_html_fragment(html);
    let Ok(link_sel) = scraper::Selector::parse(r#"a[href*="/downloadable/download/link/id/"]"#)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for link in document.select(&link_sel) {
        let Some(href) = link.value().attr("href").map(str::to_string) else {
            continue;
        };
        let option_label = decode_html(link.text().collect::<String>().trim());
        let title = link
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .find_map(|ancestor| {
                let Ok(name_sel) = scraper::Selector::parse(".product-name, strong.product-name")
                else {
                    return None;
                };
                ancestor
                    .select(&name_sel)
                    .next()
                    .map(|n| decode_html(n.text().collect::<String>().trim()))
            })
            .unwrap_or_default();
        let remaining = link
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .find_map(|row| extract_remaining_from_element(row));
        let status = if link
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .any(|el| {
                el.text()
                    .collect::<String>()
                    .to_ascii_lowercase()
                    .contains("available")
            }) {
            String::from("Available")
        } else {
            String::new()
        };
        if !href.is_empty() {
            out.push(DownloadableProduct {
                title,
                option_label,
                download_url: href,
                remaining,
                status,
            });
        }
    }
    out
}

fn extract_remaining_from_element(el: scraper::ElementRef<'_>) -> Option<u32> {
    let text = el.text().collect::<String>().to_ascii_lowercase();
    let idx = text.rfind("available")?;
    let after = &text[idx..];
    for token in after.split(|c: char| !c.is_ascii_digit()) {
        if (1..=2).contains(&token.len()) {
            if let Ok(n) = token.parse::<u32>() {
                if n <= 10 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Parse `data-product-id` rows from Browser Player `content_library` HTML.
#[must_use]
pub fn parse_library_items(html: &str) -> Vec<LibraryItem> {
    let fragment = parse_html_fragment(html);
    let Ok(item_sel) = scraper::Selector::parse("[data-product-id]") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in fragment.select(&item_sel) {
        let Some(product_id) = item
            .value()
            .attr("data-product-id")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        if !seen.insert(product_id.clone()) {
            continue;
        }
        let listen_path = find_listen_path_in_element(item);
        let title = listen_path
            .as_deref()
            .and_then(title_from_listen_path)
            .or_else(|| extract_library_title_el(item));
        out.push(LibraryItem {
            product_id,
            title,
            listen_path,
        });
    }
    out
}

fn find_listen_path_in_element(el: scraper::ElementRef<'_>) -> Option<String> {
    let Ok(sel) = scraper::Selector::parse(
        r#"a[href*="/library/player/listen/title/"], a[href*="/library/player/listen/"]"#,
    ) else {
        return None;
    };
    el.select(&sel)
        .filter_map(|a| a.value().attr("href"))
        .map(str::to_string)
        .next()
}

fn title_from_listen_path(path: &str) -> Option<String> {
    let marker = "/library/player/listen/title/";
    let idx = path.find(marker)?;
    let slug = path[idx + marker.len()..]
        .trim_matches(|c| c == '/' || c == '"' || c == '\'')
        .split('/')
        .next()
        .unwrap_or_default();
    if slug.is_empty() {
        return None;
    }
    Some(decode_html(&slug.replace('-', " ")))
}

fn extract_library_title_el(el: scraper::ElementRef<'_>) -> Option<String> {
    let Ok(sel) = scraper::Selector::parse(".product-name, .library-title, .my-library-title")
    else {
        return None;
    };
    el.select(&sel)
        .map(|n| decode_html(n.text().collect::<String>().trim()))
        .find(|t| !t.is_empty())
}

fn find_player_listen_url(html: &str, product_id: &str) -> Option<String> {
    let document = parse_html_fragment(html);
    let Ok(item_sel) = scraper::Selector::parse(&format!(r#"[data-product-id="{product_id}"]"#))
    else {
        return None;
    };
    if let Some(item) = document.select(&item_sel).next() {
        if let Some(path) = find_listen_path_in_element(item) {
            return Some(path);
        }
    }
    // Fallback: any listen title link on the page (single-title libraries).
    let Ok(any_sel) = scraper::Selector::parse(r#"a[href*="/library/player/listen/title/"]"#)
    else {
        return None;
    };
    document
        .select(&any_sel)
        .filter_map(|a| a.value().attr("href"))
        .map(str::to_string)
        .next()
}

fn extract_audio_src(html: &str) -> Option<String> {
    let document = parse_html_fragment(html);
    if let Ok(sel) = scraper::Selector::parse("audio#audio-player, #audio-player, audio") {
        for audio in document.select(&sel) {
            for attr in ["src", "data-src"] {
                if let Some(src) = audio.value().attr(attr) {
                    if src.starts_with("http") {
                        return Some(src.to_string());
                    }
                }
            }
        }
    }
    // Fallback: media CDN URL anywhere in the markup.
    for prefix in [
        "https://media.graphicaudio.net/",
        "http://media.graphicaudio.net/",
    ] {
        if let Some(idx) = html.find(prefix) {
            let from = &html[idx..];
            let end = from
                .find('"')
                .or_else(|| from.find('\''))
                .or_else(|| from.find(' '))?;
            return Some(from[..end].to_string());
        }
    }
    None
}

fn titles_match(a: &str, b: &str) -> bool {
    let na = normalize_title(a);
    let nb = normalize_title(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb || na.contains(&nb) || nb.contains(&na)
}

fn normalize_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn is_audio_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".mp3")
        || lower.ends_with(".m4a")
        || lower.ends_with(".m4b")
        || lower.ends_with(".aac")
        || lower.ends_with(".flac")
        || lower.ends_with(".ogg")
}

fn extension_from_url(url: &str) -> &'static str {
    crate::http_util::extension_from_url(url)
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_matches('.');
    if trimmed.is_empty() {
        "audio.bin".into()
    } else {
        trimmed.to_string()
    }
}

fn decode_html(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_downloadable_row() {
        let html = r#"
        <tr>
          <td>1001</td>
          <td>7/23/26</td>
          <td><strong class="product-name">Red Rising: Sons of Ares Volume 1</strong>
          <a href="https://www.graphicaudio.net/downloadable/download/link/id/ABC/"
             class="action download">M4B Zip Download, Listen with Access App and Browser Player</a>
          </td>
          <td>Available</td>
          <td>3</td>
        </tr>"#;
        let rows = parse_downloadable_products(html);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Red Rising: Sons of Ares Volume 1");
        assert!(rows[0].option_label.contains("M4B"));
        assert_eq!(rows[0].remaining, Some(3));
        assert_eq!(rows[0].format_rank(), 0);
    }

    #[test]
    fn select_prefers_m4b() {
        let rows = vec![
            DownloadableProduct {
                title: "Book".into(),
                option_label: "MP3 Zip Download".into(),
                download_url: "http://x/mp3".into(),
                remaining: Some(2),
                status: "Available".into(),
            },
            DownloadableProduct {
                title: "Book".into(),
                option_label: "M4B Zip Download".into(),
                download_url: "http://x/m4b".into(),
                remaining: Some(2),
                status: "Available".into(),
            },
        ];
        let sel = select_downloadable(&rows, "Book").unwrap();
        assert!(sel.download_url.ends_with("m4b"));
    }

    #[test]
    fn player_url_and_audio_src() {
        let html = r#"
        <tr class="my-library-item" data-product-id="5273">
          <td><a href="/library/player/listen/title/red-rising-sons-of-ares-volume-1/">Play</a></td>
        </tr>"#;
        assert_eq!(
            find_player_listen_url(html, "5273").unwrap(),
            "/library/player/listen/title/red-rising-sons-of-ares-volume-1/"
        );
        let items = parse_library_items(html);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].product_id, "5273");
        assert_eq!(
            items[0].title.as_deref(),
            Some("red rising sons of ares volume 1")
        );
        let player = r#"<audio id="audio-player" src="https://media.graphicaudio.net/app-high/x_hi.m4a" data-src="https://media.graphicaudio.net/app-high/x_hi.m4a">"#;
        assert!(extract_audio_src(player)
            .unwrap()
            .contains("app-high/x_hi.m4a"));
    }
}
