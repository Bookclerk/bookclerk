//! Liberate pipeline: license → download → decrypt → metadata → storage.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use libation_audible::{
    download_companion_pdf, download_cover_jpeg, download_licensed_audio,
    fetch_and_download_with_options, fetch_chapter_info, fetch_clips_bookmarks,
    fetch_product_metadata, open_account_client, summarize_license, AccountClient, DownloadLicense,
    DownloadOptions, DrmKind,
};
use libation_config::DownloadFormat;
use libation_config::FileTimestampMode;
use libation_decrypt::{
    brand_durations_from_chapter_info, brand_trim_range, decrypt_adrm, decrypt_cenc, encode_to_mp3,
    fixup_audiobook, parse_mp4, rebase_chapters_after_brand_trim, track_duration_ms,
    CencDecryptRequest, DecryptRequest, FixupRequest, TrimRange,
};
use libation_library::{LiberateStatus, LibraryStore};
use libation_storage::{ObjectMeta, StorageBackend};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cue::{flatten_chapters, process_chapter_titles, write_cue};
use crate::error::{LiberateError, Result};
use crate::naming::{audio_basename, sidecar_key, storage_key_with_contexts, NamingContext};
use crate::reconcile::{find_existing_for_request, StorageIndex};
use crate::split::split_audio_by_chapters;

/// Request to liberate a single title.
#[derive(Debug, Clone)]
pub struct LiberateRequest {
    pub asin: String,
    pub account_id: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub options: DownloadOptions,
    /// Root for auth files (`LIBATION_FILES_DIR`).
    pub files_dir: PathBuf,
    /// Scratch directory for encrypted + decrypted temps.
    pub cache_dir: PathBuf,
    /// When true, download even if matching media already exists in storage.
    pub force: bool,
    /// Pre-parsed license (classic `liberate --license`). Skips license API call.
    pub preloaded_license: Option<DownloadLicense>,
}

/// Result after a successful liberate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiberateResult {
    pub asin: String,
    pub storage_key: String,
    /// True when an existing file was matched and no download ran.
    pub matched_existing: bool,
}

/// Run the liberate pipeline for one book.
pub async fn liberate_book(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: LiberateRequest,
) -> Result<LiberateResult> {
    liberate_book_indexed(library, storage, req, None).await
}

/// Liberate with an optional pre-built [`StorageIndex`] (avoids re-listing storage
/// when liberating many titles). On success, newly written keys are inserted into
/// the index so later books in the same batch can match them.
pub async fn liberate_book_indexed(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: LiberateRequest,
    mut index: Option<&mut StorageIndex>,
) -> Result<LiberateResult> {
    tracing::info!(asin = %req.asin, title = %req.title, force = req.force, "liberate requested");

    if !req.force && !req.options.overwrite_existing {
        let owned_index;
        let lookup = match index.as_deref() {
            Some(idx) => idx,
            None => {
                owned_index = StorageIndex::from_storage(storage).await?;
                &owned_index
            }
        };
        if let Some(key) = find_existing_for_request(lookup, library, &req) {
            tracing::info!(
                asin = %req.asin,
                key = %key,
                "skipping download — matched existing liberated media"
            );
            library.set_liberate_status(
                &req.asin,
                &req.account_id,
                LiberateStatus::Liberated,
                Some(&key),
                None,
            )?;
            return Ok(LiberateResult {
                asin: req.asin,
                storage_key: key,
                matched_existing: true,
            });
        }
    }

    library.set_liberate_status(
        &req.asin,
        &req.account_id,
        LiberateStatus::Queued,
        None,
        None,
    )?;

    match run_pipeline(library, storage, &req).await {
        Ok(result) => {
            if let Some(idx) = index.as_mut() {
                idx.insert_key(result.storage_key.clone());
            }
            library.set_liberate_status(
                &req.asin,
                &req.account_id,
                LiberateStatus::Liberated,
                Some(&result.storage_key),
                None,
            )?;
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            let _ = library.set_liberate_status(
                &req.asin,
                &req.account_id,
                LiberateStatus::Error,
                None,
                Some(&message),
            );
            Err(err)
        }
    }
}

async fn run_pipeline(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
) -> Result<LiberateResult> {
    library.set_liberate_status(
        &req.asin,
        &req.account_id,
        LiberateStatus::Downloading,
        None,
        None,
    )?;

    let work_dir = req.cache_dir.join("liberate").join(&req.asin);
    tokio::fs::create_dir_all(&work_dir).await?;

    let (_account, download, _summary) = if let Some(license) = &req.preloaded_license {
        let account_client = open_account_client(&req.files_dir, &req.account_id).await?;
        let dest = work_dir.join(format!("{}.encrypted", req.asin));
        let download = download_licensed_audio(
            &account_client.client,
            license,
            &dest,
            req.options.download_speed_limit_kbps,
        )
        .await?;
        let summary = summarize_license(license);
        (account_client, download, summary)
    } else {
        fetch_and_download_with_options(
            &req.files_dir,
            &req.account_id,
            &req.asin,
            &req.options,
            &work_dir,
        )
        .await?
    };

    let account_client = _account;

    // Chapter metadata is needed for cues/fixup/split, and also up-front when
    // stripping Audible brand intro/outro so decrypt can trim the media.
    let need_chapters = req.options.create_cue
        || req.options.fixup_metadata
        || req.options.save_chapter_json
        || req.options.split_files_by_chapter
        || req.options.strip_audible_brand_audio;
    let chapter_info = if need_chapters {
        match fetch_chapter_info(
            &account_client.client,
            &account_client.marketplace,
            &req.asin,
            req.options.quality,
            &req.options.chapter_layout,
        )
        .await
        {
            Ok(info) => Some(info),
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "chapter metadata fetch failed");
                None
            }
        }
    } else {
        None
    };

    let brand = chapter_info
        .as_ref()
        .map(brand_durations_from_chapter_info)
        .unwrap_or_default();
    let mut runtime_ms = chapter_info.as_ref().and_then(|info| {
        info.get("runtime_length_ms")
            .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))
    });
    // Prefer chapter_info runtime; if outro trim needs length and it's missing,
    // probe the downloaded media before decrypt.
    let needs_runtime_probe =
        req.options.strip_audible_brand_audio && brand.outro_ms > 0 && runtime_ms.is_none();

    // Download first when we may need a duration probe for brand outro.
    // (fetch already happened above into `download`.)
    if needs_runtime_probe {
        match parse_mp4(&download.path) {
            Ok(mp4) => {
                let probed = track_duration_ms(&mp4.audio);
                if probed > 0 {
                    tracing::info!(
                        asin = %req.asin,
                        runtime_ms = probed,
                        "probed media duration for brand outro trim"
                    );
                    runtime_ms = Some(probed);
                }
            }
            Err(err) => {
                tracing::warn!(
                    asin = %req.asin,
                    error = %err,
                    "could not probe media duration for brand outro trim"
                );
            }
        }
    }

    let trim: Option<TrimRange> = if req.options.strip_audible_brand_audio {
        brand_trim_range(brand, runtime_ms)
    } else {
        None
    };
    if let Some(trim) = trim {
        tracing::info!(
            asin = %req.asin,
            start_ms = trim.start_ms,
            end_ms = ?trim.end_ms,
            intro_ms = brand.intro_ms,
            outro_ms = brand.outro_ms,
            "stripping Audible brand audio during decrypt"
        );
    }

    let mut liberated_path = if download.needs_decrypt {
        match download.drm_kind {
            DrmKind::Adrm => {
                let (Some(key), Some(iv)) = (&download.key, &download.iv) else {
                    return Err(LiberateError::Other(anyhow::anyhow!(
                        "aaxc download missing key/iv"
                    )));
                };
                let out = work_dir.join(format!("{}.m4b", req.asin));
                decrypt_adrm(DecryptRequest {
                    input: download.path.clone(),
                    output: out.clone(),
                    audible_key: Some(key.clone()),
                    audible_iv: Some(iv.clone()),
                    activation_bytes: None,
                    trim,
                })
                .await?;
                out
            }
            DrmKind::Widevine => {
                let (Some(kid), Some(key)) = (&download.kid, &download.cenc_key) else {
                    return Err(LiberateError::Other(anyhow::anyhow!(
                        "Widevine download missing kid/key"
                    )));
                };
                let out = work_dir.join(format!("{}.m4b", req.asin));
                decrypt_cenc(CencDecryptRequest {
                    input: download.path.clone(),
                    output: out.clone(),
                    kid: kid.clone(),
                    key: key.clone(),
                    trim,
                })
                .await?;
                out
            }
            DrmKind::Mpeg => download.path.clone(),
        }
    } else {
        download.path.clone()
    };

    let flat_chapters = chapter_info
        .as_ref()
        .map(flatten_chapters)
        .map(|ch| {
            process_chapter_titles(
                ch,
                req.options.combine_nested_chapter_titles,
                req.options.merge_opening_and_end_credits,
                req.options.strip_unabridged,
                req.options.strip_audible_brand_audio,
            )
        })
        .map(|ch| {
            if req.options.strip_audible_brand_audio && !brand.is_empty() {
                let pairs: Vec<(String, u64)> =
                    ch.iter().map(|c| (c.title.clone(), c.start_ms)).collect();
                rebase_chapters_after_brand_trim(&pairs, brand, runtime_ms)
                    .into_iter()
                    .map(|(title, start_ms)| crate::cue::FlatChapter { title, start_ms })
                    .collect()
            } else {
                ch
            }
        })
        .unwrap_or_default();

    let want_cover = req.options.download_cover || req.options.fixup_metadata;
    let cover_path = if want_cover {
        let dest = work_dir.join(format!("{}.cover.jpg", req.asin));
        match download_cover_jpeg(
            &account_client.client,
            &account_client.marketplace,
            &req.asin,
            &req.options.cover_size,
            &dest,
        )
        .await
        {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "cover download failed");
                None
            }
        }
    } else {
        None
    };

    let want_mp3 = matches!(req.options.format, DownloadFormat::Mp3);
    let will_split = req.options.split_files_by_chapter && flat_chapters.len() > 1;

    // Chapter split remuxes progressive M4B; when format=mp3, encode after split.
    // For single-file liberate, encode the whole book before fixup/store.
    if want_mp3 && !will_split {
        let ext = liberated_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.eq_ignore_ascii_case("mp3") {
            let mp3_out = work_dir.join(format!("{}.mp3", req.asin));
            encode_to_mp3(
                &liberated_path,
                &mp3_out,
                &req.options.lame,
                req.options.max_sample_rate,
            )
            .await?;
            liberated_path = mp3_out;
        }
    }

    let ext = if will_split && want_mp3 {
        "mp3".to_string()
    } else {
        liberated_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(match req.options.format {
                DownloadFormat::M4b => "m4b",
                DownloadFormat::Mp3 => "mp3",
            })
            .to_string()
    };

    if req.options.fixup_metadata && !will_split {
        let chapters: Vec<(String, u64)> = flat_chapters
            .iter()
            .map(|c| (c.title.clone(), c.start_ms))
            .collect();
        let fixed = work_dir.join(format!("{}.fixed.{}", req.asin, ext));
        match fixup_audiobook(FixupRequest {
            input: liberated_path.clone(),
            output: fixed.clone(),
            title: req.title.clone(),
            author: req.authors.clone(),
            narrator: req.narrators.clone(),
            cover: cover_path.clone(),
            chapters,
        })
        .await
        {
            Ok(outcome) => liberated_path = outcome.output,
            Err(err) => {
                tracing::warn!(
                    asin = %req.asin,
                    error = %err,
                    "metadata fixup failed; storing pre-fixup audio"
                );
            }
        }
    }

    let storage_key = if will_split {
        let total_ms = runtime_ms
            .or_else(|| {
                flat_chapters
                    .last()
                    .map(|c| c.start_ms.saturating_add(600_000))
            })
            .unwrap_or(3_600_000);
        // Brand trim already applied during decrypt; runtime_ms is pre-trim.
        let total_ms = if req.options.strip_audible_brand_audio && !brand.is_empty() {
            brand_trim_range(brand, runtime_ms)
                .and_then(|t| t.end_ms.map(|end| end.saturating_sub(t.start_ms)))
                .unwrap_or(total_ms.saturating_sub(brand.intro_ms + brand.outro_ms))
        } else {
            total_ms
        };
        let split_dir = work_dir.join("chapters");
        let file_ctx = naming_ctx(library, req);
        let folder_ctx = folder_naming_ctx(library, req);
        // Always split the progressive M4B (before any whole-file MP3 encode).
        let split_ext = "m4b";
        let chapters = split_audio_by_chapters(
            &liberated_path,
            &split_dir,
            &flat_chapters,
            total_ms,
            &folder_ctx,
            &file_ctx,
            &req.options,
            split_ext,
        )
        .await?;
        let mut first_key = String::new();
        let mut written_keys = Vec::new();
        for (idx, ch) in chapters.into_iter().enumerate() {
            let mut chapter_path = ch.path;
            let mut storage_key = ch.storage_key;
            if want_mp3 {
                let mp3_path = chapter_path.with_extension("mp3");
                encode_to_mp3(
                    &chapter_path,
                    &mp3_path,
                    &req.options.lame,
                    req.options.max_sample_rate,
                )
                .await?;
                chapter_path = mp3_path;
                storage_key = storage_key
                    .trim_end_matches(".m4b")
                    .trim_end_matches(".M4B")
                    .to_string()
                    + ".mp3";
            }
            if req.options.fixup_metadata {
                let fixed = chapter_path.with_extension(format!("fixed.{}", ext));
                // Per-chapter files: rebase chapter title only (start at 0).
                let chapter_chapters = vec![(ch.title.clone(), 0u64)];
                match fixup_audiobook(FixupRequest {
                    input: chapter_path.clone(),
                    output: fixed.clone(),
                    title: format!("{} — {}", req.title, ch.title),
                    author: req.authors.clone(),
                    narrator: req.narrators.clone(),
                    cover: cover_path.clone(),
                    chapters: chapter_chapters,
                })
                .await
                {
                    Ok(outcome) => chapter_path = outcome.output,
                    Err(err) => {
                        tracing::warn!(
                            asin = %req.asin,
                            chapter = idx + 1,
                            error = %err,
                            "chapter metadata fixup failed"
                        );
                    }
                }
            }
            let meta = object_meta_for(
                library,
                req,
                &ch.title,
                content_type_for_ext(&ext),
                tokio::fs::metadata(&chapter_path)
                    .await
                    .ok()
                    .map(|m| m.len()),
            )
            .await;
            storage.put_file(&storage_key, &chapter_path, meta).await?;
            written_keys.push(storage_key.clone());
            if first_key.is_empty() {
                first_key = storage_key;
            }
        }
        apply_storage_timestamps(storage, library, req, &written_keys).await;
        first_key
    } else {
        let storage_key = planned_storage_key_for(library, req, &ext);
        let data_len = tokio::fs::metadata(&liberated_path)
            .await
            .map(|m| m.len())
            .ok();
        let meta = object_meta_for(
            library,
            req,
            &req.title,
            content_type_for_ext(&ext),
            data_len,
        )
        .await;
        storage
            .put_file(&storage_key, &liberated_path, meta)
            .await?;
        apply_storage_timestamps(storage, library, req, std::slice::from_ref(&storage_key)).await;
        storage_key
    };

    if req.options.retain_aax_file && download.needs_decrypt {
        let aax_key = sidecar_key(&storage_key, "aaxc");
        let meta = sidecar_meta(
            &req.asin,
            &req.title,
            "audio/vnd.audible.aax",
            &download.path,
        )
        .await;
        if let Err(err) = storage.put_file(&aax_key, &download.path, meta).await {
            tracing::warn!(asin = %req.asin, error = %err, "retain aax store failed");
        }
    }

    store_artifacts(
        storage,
        &ArtifactContext {
            req,
            account: &account_client,
            audio_key: &storage_key,
            work_dir: &work_dir,
            cover_path: cover_path.as_deref(),
            chapter_info: chapter_info.as_ref(),
            flat_chapters: &flat_chapters,
            license: _summary,
        },
    )
    .await;

    if let Err(err) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!(
            path = %work_dir.display(),
            error = %err,
            "failed to clean liberate cache dir"
        );
    }

    Ok(LiberateResult {
        asin: req.asin.clone(),
        storage_key,
        matched_existing: false,
    })
}

struct ArtifactContext<'a> {
    req: &'a LiberateRequest,
    account: &'a AccountClient,
    audio_key: &'a str,
    work_dir: &'a Path,
    cover_path: Option<&'a Path>,
    chapter_info: Option<&'a Value>,
    flat_chapters: &'a [crate::cue::FlatChapter],
    license: libation_audible::LicenseSummary,
}

async fn store_artifacts(storage: &dyn StorageBackend, ctx: &ArtifactContext<'_>) {
    let req = ctx.req;
    let account = ctx.account;
    let audio_key = ctx.audio_key;
    let work_dir = ctx.work_dir;
    let cover_path = ctx.cover_path;
    let chapter_info = ctx.chapter_info;
    let flat_chapters = ctx.flat_chapters;

    if req.options.download_cover {
        if let Some(cover) = cover_path {
            let key = sidecar_key(audio_key, "jpg");
            let meta = sidecar_meta(&req.asin, &req.title, "image/jpeg", cover).await;
            if let Err(err) = storage.put_file(&key, cover, meta).await {
                tracing::warn!(asin = %req.asin, key = %key, error = %err, "cover store failed");
            }
        }
    }

    if req.options.create_cue && !flat_chapters.is_empty() {
        let cue_path = work_dir.join(format!("{}.cue", req.asin));
        let performer = req.authors.as_deref().unwrap_or("Unknown Author");
        match write_cue(
            &cue_path,
            &audio_basename(audio_key),
            performer,
            &req.title,
            flat_chapters,
        ) {
            Ok(()) => {
                let key = sidecar_key(audio_key, "cue");
                let meta =
                    sidecar_meta(&req.asin, &req.title, "application/x-cue", &cue_path).await;
                if let Err(err) = storage.put_file(&key, &cue_path, meta).await {
                    tracing::warn!(asin = %req.asin, key = %key, error = %err, "cue store failed");
                }
            }
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "cue write failed");
            }
        }
    }

    if req.options.save_chapter_json {
        if let Some(info) = chapter_info {
            let layout = chapter_layout_token(&req.options.chapter_layout);
            let json_path = work_dir.join(format!("{}.chapters.{layout}.json", req.asin));
            match tokio::fs::write(
                &json_path,
                serde_json::to_vec_pretty(info).unwrap_or_default(),
            )
            .await
            {
                Ok(()) => {
                    let key = sidecar_key(audio_key, &format!("chapters.{layout}.json"));
                    let meta =
                        sidecar_meta(&req.asin, &req.title, "application/json", &json_path).await;
                    if let Err(err) = storage.put_file(&key, &json_path, meta).await {
                        tracing::warn!(
                            asin = %req.asin,
                            key = %key,
                            error = %err,
                            "chapter json store failed"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(asin = %req.asin, error = %err, "chapter json write failed");
                }
            }
        }
    }

    if req.options.save_metadata_json {
        match fetch_product_metadata(
            &account.client,
            &account.marketplace,
            &req.asin,
            req.options.quality,
        )
        .await
        {
            Ok(meta) => {
                let json_path = work_dir.join(format!("{}.metadata.json", req.asin));
                if tokio::fs::write(
                    &json_path,
                    serde_json::to_vec_pretty(&meta).unwrap_or_default(),
                )
                .await
                .is_ok()
                {
                    let key = sidecar_key(audio_key, "metadata.json");
                    let file_meta =
                        sidecar_meta(&req.asin, &req.title, "application/json", &json_path).await;
                    if let Err(err) = storage.put_file(&key, &json_path, file_meta).await {
                        tracing::warn!(asin = %req.asin, key = %key, error = %err, "metadata.json store failed");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "catalog metadata fetch failed");
            }
        }
    }

    if req.options.download_clips_bookmarks {
        match fetch_clips_bookmarks(
            &account.client,
            &req.asin,
            None,
            None,
            ctx.license.content_format.as_deref(),
        )
        .await
        {
            Ok(Some(doc)) => {
                let json_path = work_dir.join(format!("{}.clips.json", req.asin));
                if tokio::fs::write(
                    &json_path,
                    serde_json::to_vec_pretty(&doc).unwrap_or_default(),
                )
                .await
                .is_ok()
                {
                    let key = sidecar_key(audio_key, "clips.json");
                    let file_meta =
                        sidecar_meta(&req.asin, &req.title, "application/json", &json_path).await;
                    if let Err(err) = storage.put_file(&key, &json_path, file_meta).await {
                        tracing::warn!(asin = %req.asin, key = %key, error = %err, "clips store failed");
                    }
                }
            }
            Ok(None) => tracing::debug!(asin = %req.asin, "no clips/bookmarks for title"),
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "clips/bookmarks fetch failed");
            }
        }
    }

    if req.options.download_pdf {
        let pdf_path = work_dir.join(format!("{}.pdf", req.asin));
        match download_companion_pdf(&account.client, &account.marketplace, &req.asin, &pdf_path)
            .await
        {
            Ok(Some(path)) => {
                let key = sidecar_key(audio_key, "pdf");
                let meta = sidecar_meta(&req.asin, &req.title, "application/pdf", &path).await;
                if let Err(err) = storage.put_file(&key, &path, meta).await {
                    tracing::warn!(asin = %req.asin, key = %key, error = %err, "pdf store failed");
                }
            }
            Ok(None) => tracing::debug!(asin = %req.asin, "no companion PDF for title"),
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "companion PDF download failed");
            }
        }
    }
}

async fn object_meta_for(
    library: &LibraryStore,
    req: &LiberateRequest,
    title: &str,
    content_type: &str,
    content_length: Option<u64>,
) -> ObjectMeta {
    let book = library.get_book(&req.asin, &req.account_id).ok().flatten();
    let created = resolve_timestamp(req.options.creation_time, book.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, book.as_ref());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(req.asin.clone()),
        title: Some(title.to_string()),
        creation_time: created.map(system_time_rfc3339),
        last_write_time: modified.map(system_time_rfc3339),
    }
}

fn system_time_rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339()
}

async fn sidecar_meta(asin: &str, title: &str, content_type: &str, path: &Path) -> ObjectMeta {
    let content_length = tokio::fs::metadata(path).await.ok().map(|m| m.len());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(asin.to_string()),
        title: Some(title.to_string()),
        creation_time: None,
        last_write_time: None,
    }
}

fn chapter_layout_token(layout: &str) -> &'static str {
    match layout.to_ascii_lowercase().as_str() {
        "flat" => "flat",
        _ => "tree",
    }
}

fn content_type_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "m4b" => "audio/mp4",
        "aaxc" | "aax" | "cenc" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

async fn apply_storage_timestamps(
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &LiberateRequest,
    keys: &[String],
) {
    let book = library.get_book(&req.asin, &req.account_id).ok().flatten();
    let created = resolve_timestamp(req.options.creation_time, book.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, book.as_ref());
    if created.is_none() && modified.is_none() {
        return;
    }
    for key in keys {
        if let Err(err) = storage.touch_file(key, created, modified).await {
            tracing::warn!(asin = %req.asin, key = %key, error = %err, "file timestamp update failed");
        }
    }
}

fn resolve_timestamp(
    mode: FileTimestampMode,
    book: Option<&libation_library::BookRecord>,
) -> Option<SystemTime> {
    match mode {
        FileTimestampMode::Now => Some(SystemTime::now()),
        FileTimestampMode::Purchased => book.and_then(|b| b.purchased_at).map(|dt| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp().max(0) as u64)
        }),
        FileTimestampMode::Published => book.and_then(|b| b.published_at).map(|dt| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp().max(0) as u64)
        }),
    }
}

fn naming_ctx(library: &LibraryStore, req: &LiberateRequest) -> NamingContext {
    let book = library.get_book(&req.asin, &req.account_id).ok().flatten();
    NamingContext {
        asin: req.asin.clone(),
        title: req.title.clone(),
        subtitle: book.as_ref().and_then(|b| b.subtitle.clone()),
        authors: req.authors.clone(),
        narrators: req.narrators.clone(),
        series: req
            .series
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.series.clone())),
        series_index: req
            .series_index
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.series_index.clone())),
        series_asin: book.as_ref().and_then(|b| b.series_asin.clone()),
        account_id: Some(req.account_id.clone()),
        locale: book.as_ref().map(|b| b.marketplace.clone()),
        publisher: book.as_ref().and_then(|b| b.publisher.clone()),
        categories: book.as_ref().and_then(|b| b.categories.clone()),
        length_minutes: book.as_ref().and_then(|b| b.length_minutes),
        is_abridged: book.as_ref().is_some_and(|b| b.is_abridged),
        content_kind: book.as_ref().map(|b| b.content_kind.clone()),
        ..Default::default()
    }
}

/// Folder naming context: when saving podcasts to the parent folder, evaluate
/// the folder template against the podcast parent (classic Libation behavior).
fn folder_naming_ctx(library: &LibraryStore, req: &LiberateRequest) -> NamingContext {
    let episode = naming_ctx(library, req);
    if !req.options.save_podcasts_to_parent_folder {
        return episode;
    }
    let kind = episode.content_kind.as_deref().unwrap_or("");
    if !libation_library::is_episode(kind) {
        return episode;
    }
    let Some(parent_asin) = episode.series_asin.as_deref() else {
        return episode;
    };
    let Ok(Some(parent)) = library.get_book(parent_asin, &req.account_id) else {
        return episode;
    };
    NamingContext {
        asin: parent.asin.clone(),
        title: parent.title.clone(),
        subtitle: parent.subtitle.clone(),
        authors: parent.authors.clone(),
        narrators: parent.narrators.clone(),
        series: parent.series.clone(),
        series_index: None,
        series_asin: parent.series_asin.clone(),
        account_id: Some(parent.account_id.clone()),
        locale: Some(parent.marketplace.clone()),
        publisher: parent.publisher.clone(),
        categories: parent.categories.clone(),
        length_minutes: parent.length_minutes,
        is_abridged: parent.is_abridged,
        content_kind: Some(parent.content_kind.clone()),
        ..Default::default()
    }
}

/// Planned audio storage key for `ext`, honoring folder/file templates and
/// `save_podcasts_to_parent_folder`.
#[must_use]
pub fn planned_storage_key_for(library: &LibraryStore, req: &LiberateRequest, ext: &str) -> String {
    planned_storage_key_with_rules(library, req, ext, &req.options.replacement_characters)
}

/// Like [`planned_storage_key_for`] but with an explicit replacement-rule set
/// (used by reconcile to probe wildcard patterns across sanitization profiles).
#[must_use]
pub fn planned_storage_key_with_rules(
    library: &LibraryStore,
    req: &LiberateRequest,
    ext: &str,
    replacement_rules: &[libation_config::ReplacementRule],
) -> String {
    storage_key_with_contexts(
        &folder_naming_ctx(library, req),
        &naming_ctx(library, req),
        req.options.folder_template.as_deref(),
        req.options.file_template.as_deref(),
        ext,
        replacement_rules,
    )
}

/// Compute the storage key that would be used (for dry-run / set-status).
///
/// Uses the library row (when present) so podcast episodes honor
/// `save_podcasts_to_parent_folder` the same way as a real liberate.
#[must_use]
pub fn planned_storage_key(library: &LibraryStore, req: &LiberateRequest) -> String {
    planned_storage_key_for(
        library,
        req,
        match req.options.format {
            DownloadFormat::M4b => "m4b",
            DownloadFormat::Mp3 => "mp3",
        },
    )
}

/// Download and store companion PDF only (classic `liberate --pdf`).
pub async fn liberate_pdf_only(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
) -> Result<LiberateResult> {
    let audio_key = planned_storage_key(library, req);
    let pdf_key = sidecar_key(&audio_key, "pdf");

    if !req.force {
        if let Some(book) = library.get_book(&req.asin, &req.account_id)? {
            if book.pdf_status == LiberateStatus::Liberated {
                if let Some(key) = book.pdf_storage_key {
                    return Ok(LiberateResult {
                        asin: req.asin.clone(),
                        storage_key: key,
                        matched_existing: true,
                    });
                }
            }
        }
    }

    let work_dir = req.cache_dir.join("liberate-pdf").join(&req.asin);
    tokio::fs::create_dir_all(&work_dir).await?;
    let account = libation_audible::open_account_client(&req.files_dir, &req.account_id).await?;
    let pdf_path = work_dir.join(format!("{}.pdf", req.asin));

    let Some(path) = libation_audible::download_companion_pdf(
        &account.client,
        &account.marketplace,
        &req.asin,
        &pdf_path,
    )
    .await?
    else {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "no companion PDF available for {}",
            req.asin
        )));
    };

    let meta = sidecar_meta(&req.asin, &req.title, "application/pdf", &path).await;
    storage.put_file(&pdf_key, &path, meta).await?;
    library.set_pdf_status(
        &req.asin,
        &req.account_id,
        LiberateStatus::Liberated,
        Some(&pdf_key),
    )?;

    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    Ok(LiberateResult {
        asin: req.asin.clone(),
        storage_key: pdf_key,
        matched_existing: false,
    })
}
