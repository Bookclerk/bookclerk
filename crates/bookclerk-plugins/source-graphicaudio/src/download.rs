//! Download GraphicAudio title materials into a cache dir.
//!
//! Access path is selected by `[sources.graphicaudio] access` (default `web`):
//! - `web` — Browser Player media URL (Magento session + CloudFront cookies)
//! - `zip` — Magento ZIP (M4B/MP3/FLAC); opt-in; consumes ≤3 download attempts
//! - `device` — Access App `api/links` Hi/Lo; opt-in; uses a device activation
//!
//! Override with `BOOKCLERK_GA_ACCESS` / legacy `BOOKCLERK_GA_FETCH`
//! (`web|zip|device`, plus aliases `browser` / `app`) via config env apply.

use std::path::{Path, PathBuf};

use bookclerk_source::{PlainAudioPart, PlainFetch};

use crate::client::{GraphicAudioClient, Product};
use crate::error::{GraphicAudioError, Result};
use crate::http_util::extension_from_url;
use crate::magento::{self, MagentoClient};
use crate::options::{GraphicAudioAccess, GraphicAudioContainer};

/// Env override for which GraphicAudio access path to use (legacy name).
pub const GA_FETCH_ENV: &str = "BOOKCLERK_GA_FETCH";

/// Preferred env override (`BOOKCLERK_GA_ACCESS`); falls back to [`GA_FETCH_ENV`].
pub const GA_ACCESS_ENV: &str = "BOOKCLERK_GA_ACCESS";

/// Password env (same as CLI login) for Magento ZIP / Browser Player.
pub const GA_PASSWORD_ENV: &str = "BOOKCLERK_GA_PASSWORD";

/// Read Magento password from [`GA_PASSWORD_ENV`] when set.
#[must_use]
pub fn password_from_env() -> Option<String> {
    std::env::var(GA_PASSWORD_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Fetch one product id into `cache_dir` via `api/links` Hi/Lo URLs.
pub async fn fetch_title_materials(
    client: &GraphicAudioClient,
    product_id: &str,
    cache_dir: &Path,
) -> Result<PlainFetch> {
    fetch_title_materials_with_quality(client, product_id, cache_dir, true).await
}

/// Like [`fetch_title_materials`], but selects Hi vs Lo from source bitrate.
pub async fn fetch_title_materials_with_quality(
    client: &GraphicAudioClient,
    product_id: &str,
    cache_dir: &Path,
    prefer_hi: bool,
) -> Result<PlainFetch> {
    fetch_access_app(client, product_id, cache_dir, prefer_hi).await
}

/// Parameters for [`fetch_title_with_mode`].
#[derive(Debug, Clone)]
pub struct TitleFetchRequest<'a> {
    pub store_base_url: &'a str,
    pub email: &'a str,
    pub product_id: &'a str,
    pub product_title: Option<&'a str>,
    pub cache_dir: &'a Path,
    pub prefer_hi: bool,
    pub mode: GraphicAudioAccess,
    pub password: Option<&'a str>,
    /// ZIP SKU preference when [`Self::mode`] is [`GraphicAudioAccess::Zip`].
    pub zip_container: GraphicAudioContainer,
}

/// Fetch owned audio for one product using the configured access path only
/// (no ZIP→web→device cascade).
pub async fn fetch_title_with_mode(
    access: &GraphicAudioClient,
    req: TitleFetchRequest<'_>,
) -> Result<PlainFetch> {
    let title_dir = req.cache_dir.join(req.product_id);
    std::fs::create_dir_all(&title_dir)?;

    match req.mode {
        GraphicAudioAccess::Zip => {
            let password = req.password.ok_or_else(|| {
                GraphicAudioError::auth(format!(
                    "GraphicAudio access=zip requires {GA_PASSWORD_ENV} for Magento storefront access"
                ))
            })?;
            fetch_magento_zip(
                req.store_base_url,
                req.email,
                password,
                req.product_id,
                req.product_title,
                &title_dir,
                req.zip_container,
            )
            .await
        }
        GraphicAudioAccess::Web => {
            let password = req.password.ok_or_else(|| {
                GraphicAudioError::auth(format!(
                    "GraphicAudio access=web requires {GA_PASSWORD_ENV} for Magento storefront access"
                ))
            })?;
            fetch_browser(
                req.store_base_url,
                req.email,
                password,
                req.product_id,
                &title_dir,
            )
            .await
        }
        GraphicAudioAccess::Device => {
            fetch_access_app(access, req.product_id, req.cache_dir, req.prefer_hi).await
        }
    }
}

async fn fetch_magento_zip(
    store_base_url: &str,
    email: &str,
    password: &str,
    product_id: &str,
    product_title: Option<&str>,
    title_dir: &Path,
    container: GraphicAudioContainer,
) -> Result<PlainFetch> {
    let title = product_title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            GraphicAudioError::download(format!(
                "Magento ZIP needs a product title to match downloadable rows (product {product_id})"
            ))
        })?;

    let client = MagentoClient::new(store_base_url)?;
    client.login(email, password).await?;
    let audio_path = magento::fetch_zip_for_title(&client, title, title_dir, container).await?;
    Ok(plain_from_audio_path(audio_path))
}

async fn fetch_browser(
    store_base_url: &str,
    email: &str,
    password: &str,
    product_id: &str,
    title_dir: &Path,
) -> Result<PlainFetch> {
    let client = MagentoClient::new(store_base_url)?;
    client.login(email, password).await?;
    let audio_path = magento::fetch_browser_audio(&client, product_id, title_dir).await?;
    Ok(plain_from_audio_path(audio_path))
}

async fn fetch_access_app(
    client: &GraphicAudioClient,
    product_id: &str,
    cache_dir: &Path,
    prefer_hi: bool,
) -> Result<PlainFetch> {
    std::fs::create_dir_all(cache_dir)?;
    let title_dir = cache_dir.join(product_id);
    std::fs::create_dir_all(&title_dir)?;

    let links = client.links(product_id).await?;
    let url = links.url_for_quality(prefer_hi).ok_or_else(|| {
        GraphicAudioError::download(format!("no Lo/Hi download URL for product {product_id}"))
    })?;

    let ext = extension_from_url(url);
    let path = title_dir.join(format!("audio{ext}"));
    client.download_to_path(url, &path).await?;

    Ok(PlainFetch {
        parts: vec![PlainAudioPart {
            path,
            title: None,
            duration_ms: None,
        }],
        m4b_path: None,
        cover_path: None,
        chapters: Vec::new(),
        pdf_url: None,
    })
}

fn plain_from_audio_path(path: PathBuf) -> PlainFetch {
    let is_m4b = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("m4b"));
    if is_m4b {
        PlainFetch {
            parts: Vec::new(),
            m4b_path: Some(path),
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: None,
        }
    } else {
        PlainFetch {
            parts: vec![PlainAudioPart {
                path,
                title: None,
                duration_ms: None,
            }],
            m4b_path: None,
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: None,
        }
    }
}

/// Resolve a display title for Magento ZIP matching from Access App products.
pub async fn product_title_for(
    client: &GraphicAudioClient,
    product_id: &str,
) -> Result<Option<String>> {
    let products = client.products().await?;
    Ok(products
        .into_iter()
        .find(|p| p.id == product_id)
        .map(|p: Product| p.display_title()))
}
