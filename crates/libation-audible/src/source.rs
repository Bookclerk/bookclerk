//! [`ContentSource`] adapter for Audible.

use std::path::Path;

use async_trait::async_trait;
use libation_library::LibraryStore;
use libation_source::{
    ContentSource, EncryptedDrmKind, EncryptedFetch, FetchOptions, LoginOptions, Result,
    ScanOptions, ScanSummary, SourceAccount, SourceError, SourceFetch, SourceKind,
};

use crate::accounts::list_accounts;
use crate::artifacts::{download_cover_jpeg, fetch_chapter_info};
use crate::auth::{begin_login, AuthLoginOptions, LoginMode};
use crate::download::{fetch_and_download_with_options, DrmKind};
use crate::error::AudibleError;
use crate::sync::scan_library;

/// Audible store as a [`ContentSource`].
#[derive(Debug, Default, Clone, Copy)]
pub struct AudibleSource;

impl AudibleSource {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContentSource for AudibleSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Audible
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
            source: SourceKind::Audible,
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
                source: SourceKind::Audible,
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
        let (account, download, _summary) = fetch_and_download_with_options(
            files_dir,
            account_id,
            title_id,
            &opts.download,
            &opts.cache_dir,
        )
        .await
        .map_err(map_audible_err)?;

        let need_chapters = opts.download.create_cue
            || opts.download.fixup_metadata
            || opts.download.save_chapter_json
            || opts.download.split_files_by_chapter
            || opts.download.strip_audible_brand_audio;
        let mut chapter_info = None;
        if need_chapters {
            match fetch_chapter_info(
                &account.client,
                &account.marketplace,
                title_id,
                opts.download.quality,
                &opts.download.chapter_layout,
            )
            .await
            {
                Ok(info) => chapter_info = Some(info),
                Err(err) => {
                    tracing::warn!(asin = %title_id, error = %err, "chapter metadata fetch failed");
                }
            }
        }

        let want_cover = opts.download.download_cover || opts.download.fixup_metadata;
        let mut cover_path = None;
        if want_cover {
            let dest = opts.cache_dir.join(format!("{title_id}.cover.jpg"));
            match download_cover_jpeg(
                &account.client,
                &account.marketplace,
                title_id,
                &opts.download.cover_size,
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
