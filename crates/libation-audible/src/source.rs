//! [`ContentSource`] adapter for Audible.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use libation_config::{AudioQuality, Config};
use libation_library::LibraryStore;
use libation_source::{
    ContentSource, EncryptedDrmKind, EncryptedFetch, FetchOptions, LoginOptions, PortalAuthMode,
    Result, ScanOptions, ScanSummary, SourceAccount, SourceBrand, SourceError, SourceFetch,
    SourceRegistry,
};

use crate::accounts::list_accounts;
use crate::artifacts::{download_cover_jpeg, fetch_chapter_info};
use crate::auth::{begin_login, AuthLoginOptions, LoginMode};
use crate::download::{fetch_and_download_with_options, DrmKind};
use crate::error::AudibleError;
use crate::sync::scan_library;

/// Canonical plugin id.
pub const ID: &str = "audible";

const AUTH_SUFFIXES: &[&str] = &[".auth", ".wvd"];

/// Audible store as a [`ContentSource`].
#[derive(Debug, Default, Clone, Copy)]
pub struct AudibleSource {
    /// License bitrate tier (`[sources.audible] bitrate`).
    pub bitrate: AudioQuality,
}

impl AudibleSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `[sources.audible]` knobs from config.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let bitrate = config
            .sources
            .get_string(ID, "bitrate")
            .and_then(parse_audio_quality)
            .unwrap_or_default();
        Self { bitrate }
    }

    #[must_use]
    pub fn with_bitrate(mut self, bitrate: AudioQuality) -> Self {
        self.bitrate = bitrate;
        self
    }
}

#[async_trait]
impl ContentSource for AudibleSource {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Audible"
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Oauth
    }

    fn portal_brand(&self) -> SourceBrand {
        SourceBrand {
            id: "audible",
            name: "Audible",
            bg: "#F8991D",
            fg: "#111111",
            accent: "#D97706",
            icon_url: "https://www.google.com/s2/favicons?domain=audible.com&sz=128",
        }
    }

    fn auth_credential_suffixes(&self) -> &'static [&'static str] {
        AUTH_SUFFIXES
    }

    fn sort_key(&self) -> u32 {
        0
    }

    fn supports_preloaded_license(&self) -> bool {
        true
    }

    async fn login(&self, files_dir: &Path, opts: LoginOptions) -> Result<SourceAccount> {
        // Audible ignores email/password (OAuth via browser/QR). Drive the
        // interactive LoginServer flow with marketplace / label / force.
        let marketplace = if opts.marketplace.trim().is_empty() {
            String::from("us")
        } else {
            opts.marketplace
        };
        let auth_opts = AuthLoginOptions {
            marketplace,
            label: opts.label,
            files_dir: files_dir.to_path_buf(),
            mode: LoginMode::Server,
            force: opts.force,
            ..AuthLoginOptions::default()
        };
        let session = begin_login(auth_opts, |_| {})
            .await
            .map_err(map_audible_err)?;
        Ok(SourceAccount {
            account_id: session.account_id,
            source: ID.into(),
            marketplace: session.marketplace,
            label: session.label,
            scan_enabled: true,
        })
    }

    async fn list_accounts(&self, files_dir: &Path) -> Result<Vec<SourceAccount>> {
        let accounts = list_accounts(files_dir).await.map_err(map_audible_err)?;
        Ok(accounts
            .into_iter()
            .map(|a| SourceAccount {
                account_id: a.account_id,
                source: ID.into(),
                marketplace: a.marketplace,
                label: a.label,
                scan_enabled: true,
            })
            .collect())
    }

    async fn scan(
        &self,
        files_dir: &Path,
        library: &LibraryStore,
        opts: ScanOptions,
    ) -> Result<ScanSummary> {
        scan_library(files_dir, library, opts)
            .await
            .map_err(map_audible_err)
    }

    async fn fetch_title(
        &self,
        files_dir: &Path,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> Result<SourceFetch> {
        let mut dl = opts.download.clone();
        dl.quality = self.bitrate;

        let (account, download, _summary) =
            fetch_and_download_with_options(files_dir, account_id, title_id, &dl, &opts.cache_dir)
                .await
                .map_err(map_audible_err)?;

        let need_chapters = dl.create_cue
            || dl.fixup_metadata
            || dl.wants_chapter_json()
            || dl.wants_split_by_chapter()
            || dl.strip_audible_brand_audio;
        let mut chapter_info = None;
        if need_chapters {
            match fetch_chapter_info(
                &account.client,
                &account.marketplace,
                title_id,
                dl.quality,
                &dl.chapter_layout,
            )
            .await
            {
                Ok(info) => chapter_info = Some(info),
                Err(err) => {
                    tracing::warn!(asin = %title_id, error = %err, "chapter metadata fetch failed");
                }
            }
        }

        let want_cover = dl.download_cover || dl.fixup_metadata;
        let mut cover_path = None;
        if want_cover {
            let dest = opts.cache_dir.join(format!("{title_id}.cover.jpg"));
            match download_cover_jpeg(
                &account.client,
                &account.marketplace,
                title_id,
                &dl.cover_size,
                &dest,
            )
            .await
            {
                Ok(path) => cover_path = path,
                Err(err) => {
                    tracing::warn!(asin = %title_id, error = %err, "cover download failed");
                }
            }
        }

        Ok(SourceFetch::Encrypted(EncryptedFetch {
            path: download.path,
            drm_kind: map_drm(download.drm_kind),
            key: download.key,
            iv: download.iv,
            kid: download.kid,
            cenc_key: download.cenc_key,
            needs_decrypt: download.needs_decrypt,
            pdf_url: download.pdf_url,
            content_format: download.content_format,
            chapter_info,
            cover_path,
            product_metadata: None,
            clips_bookmarks: None,
        }))
    }

    fn config_options(&self) -> &'static [libation_source::SourceConfigOption] {
        AUDIBLE_CONFIG_OPTIONS
    }
}

const AUDIBLE_CONFIG_OPTIONS: &[libation_source::SourceConfigOption] =
    &[libation_source::SourceConfigOption {
        key: "bitrate",
        label: "Bitrate",
        values: &[
            libation_source::ConfigOptionValue {
                id: "high",
                label: "High",
            },
            libation_source::ConfigOptionValue {
                id: "normal",
                label: "Normal",
            },
        ],
    }];

fn parse_audio_quality(raw: &str) -> Option<AudioQuality> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "high" => Some(AudioQuality::High),
        "normal" => Some(AudioQuality::Normal),
        _ => None,
    }
}

fn map_drm(kind: DrmKind) -> EncryptedDrmKind {
    match kind {
        DrmKind::Adrm => EncryptedDrmKind::Adrm,
        DrmKind::Widevine => EncryptedDrmKind::Widevine,
        DrmKind::Mpeg => EncryptedDrmKind::Mpeg,
    }
}

fn map_audible_err(err: AudibleError) -> SourceError {
    match err {
        AudibleError::NoAccounts(msg) => SourceError::NoAccounts(msg),
        AudibleError::Auth(msg) | AudibleError::AccountNotFound(msg) => SourceError::Auth(msg),
        AudibleError::Io(err) => SourceError::Io(err),
        AudibleError::Other(err) => SourceError::Other(err),
        other => SourceError::Api(other.to_string()),
    }
}

/// Parse `[sources.audible]` into an [`AudibleSource`].
#[must_use]
pub fn from_config(config: &Config) -> AudibleSource {
    AudibleSource::from_config(config)
}

/// Register Audible when `[sources.audible] enabled` (default true).
pub fn register(registry: &mut SourceRegistry, config: &Config) {
    if config.sources.is_enabled(ID) {
        registry.register(Arc::new(from_config(config)));
    }
}
