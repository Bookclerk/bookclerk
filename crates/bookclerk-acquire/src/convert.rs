//! Convert acquired m4b/m4a to mp3 (classic LibationCli: `convert`).

use std::path::PathBuf;

use bookclerk_library::{AcquireStatus, BookRecord, LibraryStore};
use bookclerk_media::encode_to_mp3;
use bookclerk_storage::{ObjectMeta, StorageBackend};

use crate::error::{AcquireError, Result};
use crate::naming::swap_audio_extension;

/// Options for [`convert_book`].
#[derive(Debug, Clone)]
pub struct ConvertRequest {
    /// Scratch directory for temporary acquire/convert files.
    pub cache_dir: PathBuf,
    /// When true, re-download or re-convert even if output exists.
    pub force: bool,
    /// LAME MP3 encoder settings from config.
    pub lame: bookclerk_config::LameConfig,
    /// Optional ceiling on output sample rate in Hz.
    pub max_sample_rate: Option<u32>,
}

/// Summary of a batch convert run.
#[derive(Debug, Clone, Default)]
pub struct ConvertSummary {
    /// Titles successfully converted in this run.
    pub converted: u32,
    /// Titles skipped (already done or ineligible).
    pub skipped: u32,
    /// Titles that failed conversion or matching.
    pub failed: u32,
}

/// Convert one acquired m4b/m4a to mp3 and update the library storage key.
///
/// # Arguments
///
/// * `library` - Library store used to update acquire status / storage key.
/// * `storage` - Object storage backend holding the source and destination objects.
/// * `book` - Acquired book row whose `storage_key` points at m4b/m4a.
/// * `req` - Cache directory, force flag, and LAME settings.
///
/// # Returns
///
/// Object-storage key of the written MP3.
///
/// # Errors
///
/// Returns [`AcquireError`] when the source is missing/ineligible, encode fails,
/// or library/storage updates fail.
pub async fn convert_book(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    book: &BookRecord,
    req: &ConvertRequest,
) -> Result<String> {
    let title_id = book.title_id();
    let key = book
        .storage_key
        .as_ref()
        .ok_or_else(|| AcquireError::Other(anyhow::anyhow!("{title_id}: no storage_key")))?;
    let ext = key.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "m4b" | "m4a") {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "{title_id}: not an m4b/m4a file ({ext})",
        )));
    }

    let mp3_key = swap_audio_extension(key, "mp3");
    if !req.force && storage.exists(&mp3_key).await? {
        library
            .set_acquire_status(
                book.title_id(),
                &book.account_id,
                AcquireStatus::Acquired,
                Some(&mp3_key),
                None,
            )
            .await?;
        return Ok(mp3_key);
    }

    let file_id = book.asin_or_isbn();
    let work_dir = req.cache_dir.join("convert").join(file_id);
    tokio::fs::create_dir_all(&work_dir).await?;
    let input = work_dir.join(format!("{file_id}.{ext}"));
    let output = work_dir.join(format!("{file_id}.mp3"));

    let data = storage.get(key).await?;
    tokio::fs::write(&input, &data).await?;
    encode_to_mp3(&input, &output, &req.lame, req.max_sample_rate).await?;

    let meta = ObjectMeta {
        content_type: Some("audio/mpeg".into()),
        content_length: tokio::fs::metadata(&output).await.ok().map(|m| m.len()),
        asin: Some(file_id.to_string()),
        title: Some(book.title.clone()),
        creation_time: None,
        last_write_time: None,
    };
    storage.put_file(&mp3_key, &output, meta).await?;

    if mp3_key != *key {
        let _ = storage.delete(key).await;
    }

    library
        .set_acquire_status(
            book.title_id(),
            &book.account_id,
            AcquireStatus::Acquired,
            Some(&mp3_key),
            None,
        )
        .await?;

    if let Err(err) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!(
            path = %work_dir.display(),
            error = %err,
            "failed to clean convert cache dir"
        );
    }

    Ok(mp3_key)
}
