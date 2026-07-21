//! Shared types for content sources.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::options::DownloadOptions;

/// Which store a title / account belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Audible,
    LibroFm,
}

impl SourceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audible => "audible",
            Self::LibroFm => "libro",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "audible" => Some(Self::Audible),
            "libro" | "librofm" | "libro.fm" => Some(Self::LibroFm),
            _ => None,
        }
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Account discovered or created by a content source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceAccount {
    pub account_id: String,
    pub source: SourceKind,
    pub marketplace: String,
    pub label: Option<String>,
    pub scan_enabled: bool,
}

/// Options for interactive / CLI login.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub marketplace: String,
    pub label: Option<String>,
    /// Libro.fm email (ignored for Audible OAuth).
    pub email: Option<String>,
    /// Libro.fm password (ignored for Audible OAuth).
    pub password: Option<String>,
    pub force: bool,
}

/// Options for a library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account nicknames / ids.
    pub accounts: Vec<String>,
    pub page_size: u32,
    /// Import podcast episodes (`ImportEpisodes`) — Audible-only.
    pub import_episodes: bool,
    /// Import Audible Plus / non-owned titles — Audible-only.
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
    /// Chapter info JSON from Audible content metadata (optional).
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
