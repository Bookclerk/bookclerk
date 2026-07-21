//! Convert liberated m4b/m4a to mp3 (classic LibationCli: `convert`).

use std::path::PathBuf;

use libation_decrypt::encode_to_mp3;
use libation_library::{BookRecord, LiberateStatus, LibraryStore};
use libation_storage::{ObjectMeta, StorageBackend};

use crate::error::{LiberateError, Result};
use crate::naming::swap_audio_extension;

/// Options for [`convert_book`].
#[derive(Debug, Clone)]
pub struct ConvertRequest {
    pub cache_dir: PathBuf,
    pub force: bool,
    pub lame: libation_config::LameConfig,
    pub max_sample_rate: Option<u32>,
}

/// Summary of a batch convert run.
#[derive(Debug, Clone, Default)]
pub struct ConvertSummary {
    pub converted: u32,
    pub skipped: u32,
    pub failed: u32,
}

/// Convert one liberated m4b/m4a to mp3 and update the library storage key.
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
        .ok_or_else(|| LiberateError::Other(anyhow::anyhow!("{title_id}: no storage_key")))?;
    let ext = key.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "m4b" | "m4a") {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "{title_id}: not an m4b/m4a file ({ext})",
        )));
    }

    let mp3_key = swap_audio_extension(key, "mp3");
    if !req.force && storage.exists(&mp3_key).await? {
        library.set_liberate_status(
            book.title_id(),
            &book.account_id,
            LiberateStatus::Liberated,
            Some(&mp3_key),
            None,
        )?;
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

    library.set_liberate_status(
        book.title_id(),
        &book.account_id,
        LiberateStatus::Liberated,
        Some(&mp3_key),
        None,
    )?;

    if let Err(err) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!(
            path = %work_dir.display(),
            error = %err,
            "failed to clean convert cache dir"
        );
    }

    Ok(mp3_key)
}
