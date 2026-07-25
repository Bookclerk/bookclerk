//! Shared types for content sources.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::options::DownloadOptions;

/// One allowed value for a [`SourceConfigOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigOptionValue {
    /// Wire / TOML id (`high`, `m4b`, `web`, …).
    pub id: &'static str,
    /// Human label.
    pub label: &'static str,
}

/// One source-native config knob under `[sources.<id>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceConfigOption {
    /// TOML key (`bitrate`, `container`, `access`).
    pub key: &'static str,
    /// Human label.
    pub label: &'static str,
    /// Allowed values.
    pub values: &'static [ConfigOptionValue],
}

/// Account discovered or created by a content source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceAccount {
    pub account_id: String,
    /// Canonical plugin id (`audible`, `libro`, …).
    pub source: String,
    pub marketplace: String,
    pub label: Option<String>,
    pub scan_enabled: bool,
}

/// Options for interactive / CLI login.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub marketplace: String,
    pub label: Option<String>,
    /// Email/password sources; ignored for OAuth.
    pub email: Option<String>,
    /// Email/password sources; ignored for OAuth.
    pub password: Option<String>,
    pub force: bool,
}

/// Options for a library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account nicknames / ids.
    pub accounts: Vec<String>,
    pub page_size: u32,
    /// Import podcast episodes — consumed by plugins that support it.
    pub import_episodes: bool,
    /// Import catalog Plus / non-owned titles — consumed by plugins that support it.
    pub import_plus_titles: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            page_size: 50,
            import_episodes: true,
            import_plus_titles: true,
        }
    }
}

/// Summary of a scan run.
#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub accounts: usize,
    pub books_upserted: usize,
    pub pages: u32,
    pub skipped_disabled: usize,
}

impl ScanSummary {
    /// Merge another source's summary into this one.
    pub fn merge(&mut self, other: &Self) {
        self.accounts += other.accounts;
        self.books_upserted += other.books_upserted;
        self.pages += other.pages;
        self.skipped_disabled += other.skipped_disabled;
    }
}

/// Options passed to [`crate::ContentSource::fetch_title`].
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub download: DownloadOptions,
    pub cache_dir: PathBuf,
}

/// One DRM-free audio part (chapter file or single book).
#[derive(Debug, Clone)]
pub struct PlainAudioPart {
    pub path: PathBuf,
    pub title: Option<String>,
    /// Duration in milliseconds when known.
    pub duration_ms: Option<u64>,
}

/// DRM-free fetch result (Libro.fm and similar).
#[derive(Debug, Clone)]
pub struct PlainFetch {
    pub parts: Vec<PlainAudioPart>,
    /// Pre-built M4B from the store when available.
    pub m4b_path: Option<PathBuf>,
    pub cover_path: Option<PathBuf>,
    pub chapters: Vec<(String, u64)>,
}

/// Encrypted Audible-style download ready for decrypt.
#[derive(Debug, Clone)]
pub struct EncryptedFetch {
    pub path: PathBuf,
    pub drm_kind: EncryptedDrmKind,
    pub key: Option<String>,
    pub iv: Option<String>,
    pub kid: Option<String>,
    pub cenc_key: Option<String>,
    pub needs_decrypt: bool,
    pub pdf_url: Option<String>,
    pub content_format: Option<String>,
    /// Chapter info JSON from content metadata (optional).
    pub chapter_info: Option<serde_json::Value>,
    pub cover_path: Option<PathBuf>,
    pub product_metadata: Option<serde_json::Value>,
    pub clips_bookmarks: Option<serde_json::Value>,
}

/// DRM kind for encrypted fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptedDrmKind {
    Adrm,
    Widevine,
    Mpeg,
}

/// Result of fetching a title for liberate.
#[derive(Debug, Clone)]
pub enum SourceFetch {
    Encrypted(EncryptedFetch),
    Plain(PlainFetch),
}
