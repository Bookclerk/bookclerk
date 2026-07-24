//! Liberate pipeline: license → download → decrypt → metadata → storage.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Datelike;
use libation_audible::{
    download_companion_pdf, download_cover_jpeg, download_licensed_audio,
    fetch_and_download_with_options, fetch_chapter_info, fetch_clips_bookmarks,
    fetch_product_metadata, open_account_client, summarize_license, AccountClient, DownloadLicense,
    DownloadOptions, DrmKind,
};
use libation_config::DownloadFormat;
use libation_config::FileTimestampMode;
use libation_decrypt::{
    align_chapter_starts, brand_durations_from_chapter_info, brand_trim_range, decrypt_adrm,
    decrypt_cenc, encode_to_mp3, fixup_audiobook, libation_tool_tag, package_m4b_from_mp3,
    parse_mp4, rebase_chapters_after_brand_trim, runtime_length_ms_from_chapter_info,
    track_duration_ms, CencDecryptRequest, ChapterAlignOptions, DecryptRequest, FixupRequest,
    PackageM4bRequest, TrimRange,
};
use libation_enrich::{fetch_audnexus_book, fetch_public_chapter_info};
use libation_library::{LiberateStatus, LibraryStore};
use libation_source::{
    ContentSource, EncryptedDrmKind, EncryptedFetch, FetchOptions, PlainFetch, SourceFetch,
    SourceKind,
};
use libation_storage::{ObjectMeta, StorageBackend};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cue::{
    apply_start_map_to_chapter_tree, chapters_from_audible_info_for_plain_audio, flatten_chapters,
    process_chapter_titles, rebase_chapter_tree_for_plain_audio, write_cue, FlatChapter,
};
use crate::error::{LiberateError, Result};
use crate::naming::{audio_basename, sidecar_key, storage_key_with_contexts, NamingContext};
use crate::reconcile::{find_existing_for_request, StorageIndex};
use crate::split::split_audio_by_chapters;

/// Request to liberate a single title.
#[derive(Debug, Clone)]
pub struct LiberateRequest {
    /// Download product id (Audible ASIN / Libro ISBN).
    pub asin: String,
    /// Stable library UUID when known (preferred for status updates).
    pub book_uuid: Option<String>,
    /// Which store owns this title.
    pub source: SourceKind,
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
    liberate_book_indexed(library, storage, req, None, None).await
}

/// Liberate with an optional pre-built [`StorageIndex`] (avoids re-listing storage
/// when liberating many titles). On success, newly written keys are inserted into
/// the index so later books in the same batch can match them.
///
/// When `source` is `Some`, fetch goes through [`ContentSource::fetch_title`]
/// (Encrypted → decrypt path, Plain → M4B packaging / MP3 handling).
/// When `None`, Audible titles use the legacy direct Audible download path;
/// non-Audible titles require a `source`.
pub async fn liberate_book_indexed(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: LiberateRequest,
    mut index: Option<&mut StorageIndex>,
    source: Option<&dyn ContentSource>,
) -> Result<LiberateResult> {
    tracing::info!(
        asin = %req.asin,
        source = %req.source,
        title = %req.title,
        force = req.force,
        output = ?req.options.effective_output(),
        "liberate requested"
    );

    if req.options.wants_opus() {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "output=opus is not implemented yet; use enriched_m4b, single_mp3, \
             split_mp3_by_chapter, or none"
        )));
    }
    if req.options.wants_split_by_size() {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "output=split_mp3_by_size is not implemented yet (split_mp3_max_mb={}); \
             use split_mp3_by_chapter or single_mp3",
            req.options.split_mp3_max_mb
        )));
    }

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
                status_key(&req),
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
        status_key(&req),
        &req.account_id,
        LiberateStatus::Queued,
        None,
        None,
    )?;

    match run_pipeline(library, storage, &req, source).await {
        Ok(result) => {
            if let Some(idx) = index.as_mut() {
                idx.insert_key(result.storage_key.clone());
            }
            library.set_liberate_status(
                status_key(&req),
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
                status_key(&req),
                &req.account_id,
                LiberateStatus::Error,
                None,
                Some(&message),
            );
            Err(err)
        }
    }
}

fn status_key(req: &LiberateRequest) -> &str {
    req.book_uuid.as_deref().unwrap_or(&req.asin)
}

async fn run_pipeline(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    source: Option<&dyn ContentSource>,
) -> Result<LiberateResult> {
    library.set_liberate_status(
        status_key(req),
        &req.account_id,
        LiberateStatus::Downloading,
        None,
        None,
    )?;

    // Prefer ContentSource when provided. For Audible with a preloaded license,
    // keep the legacy license path (ContentSource does not accept vouchers).
    if let Some(source) = source {
        if req.preloaded_license.is_none() {
            return run_source_pipeline(library, storage, req, source).await;
        }
    }

    if req.source != SourceKind::Audible {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "content source `{}` required to liberate title {}",
            req.source,
            req.asin
        )));
    }

    run_audible_pipeline(library, storage, req).await
}

async fn run_source_pipeline(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    source: &dyn ContentSource,
) -> Result<LiberateResult> {
    let work_dir = req.cache_dir.join("liberate").join(status_key(req));
    tokio::fs::create_dir_all(&work_dir).await?;

    let fetch = source
        .fetch_title(
            &req.files_dir,
            &req.account_id,
            &req.asin,
            &FetchOptions {
                download: req.options.clone(),
                cache_dir: work_dir.clone(),
            },
        )
        .await?;

    match fetch {
        SourceFetch::Plain(plain) => {
            store_plain_fetch(library, storage, req, &work_dir, plain).await
        }
        SourceFetch::Encrypted(enc) => {
            store_encrypted_fetch(library, storage, req, &work_dir, enc).await
        }
    }
}

async fn store_encrypted_fetch(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    work_dir: &Path,
    download: EncryptedFetch,
) -> Result<LiberateResult> {
    let brand = download
        .chapter_info
        .as_ref()
        .map(brand_durations_from_chapter_info)
        .unwrap_or_default();
    let mut runtime_ms = download
        .chapter_info
        .as_ref()
        .and_then(runtime_length_ms_from_chapter_info);
    if req.options.strip_audible_brand_audio && brand.outro_ms > 0 && runtime_ms.is_none() {
        if let Ok(mp4) = parse_mp4(&download.path) {
            let probed = track_duration_ms(&mp4.audio);
            if probed > 0 {
                runtime_ms = Some(probed);
            }
        }
    }
    let trim = if req.options.strip_audible_brand_audio {
        brand_trim_range(brand, runtime_ms)
    } else {
        None
    };

    let mut liberated_path = if download.needs_decrypt {
        match download.drm_kind {
            EncryptedDrmKind::Adrm => {
                let (Some(key), Some(iv)) = (&download.key, &download.iv) else {
                    return Err(LiberateError::Other(anyhow::anyhow!(
                        "aaxc download missing key/iv"
                    )));
                };
                let out = work_dir.join(format!("{}.m4b", status_key(req)));
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
            EncryptedDrmKind::Widevine => {
                let (Some(kid), Some(key)) = (&download.kid, &download.cenc_key) else {
                    return Err(LiberateError::Other(anyhow::anyhow!(
                        "Widevine download missing kid/key"
                    )));
                };
                let out = work_dir.join(format!("{}.m4b", status_key(req)));
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
            EncryptedDrmKind::Mpeg => download.path.clone(),
        }
    } else {
        download.path.clone()
    };

    let flat_chapters = download
        .chapter_info
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

    let want_mp3 = req.options.wants_mp3();
    let will_split = req.options.wants_split_by_chapter() && flat_chapters.len() > 1;
    if want_mp3 && !will_split {
        let ext = liberated_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.eq_ignore_ascii_case("mp3") {
            let mp3_out = work_dir.join(format!("{}.mp3", status_key(req)));
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

    let chapters: Vec<(String, u64)> = flat_chapters
        .iter()
        .map(|c| (c.title.clone(), c.start_ms))
        .collect();
    let cover_path = download.cover_path.clone();
    let ext = liberated_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("m4b")
        .to_string();

    if req.options.fixup_metadata && !will_split {
        let fixed = work_dir.join(format!("{}.fixed.{}", status_key(req), ext));
        // Audible chapters are rebased/title-processed; always replace embedded
        // chpl/tracks so brand-trim alignment reaches the stored file.
        match fixup_audiobook(build_fixup_request(
            library,
            req,
            liberated_path.clone(),
            fixed.clone(),
            cover_path.clone(),
            chapters,
            None,
            !flat_chapters.is_empty(),
            &PlainAudibleCatalog::default(),
        ))
        .await
        {
            Ok(outcome) => liberated_path = outcome.output,
            Err(err) => {
                tracing::warn!(
                    id = %status_key(req),
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
        let split_dir = work_dir.join("chapters");
        let chapters = split_audio_by_chapters(
            &liberated_path,
            &split_dir,
            &flat_chapters,
            total_ms,
            &folder_naming_ctx(library, req),
            &naming_ctx(library, req),
            &req.options,
            "m4b",
        )
        .await?;
        let mut first_key = String::new();
        let mut written_keys = Vec::new();
        for ch in chapters {
            let mut chapter_path = ch.path;
            let mut key = ch.storage_key;
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
                key = key
                    .trim_end_matches(".m4b")
                    .trim_end_matches(".M4B")
                    .to_string()
                    + ".mp3";
            }
            let meta = object_meta_for(
                library,
                req,
                &ch.title,
                content_type_for_ext(if want_mp3 { "mp3" } else { "m4b" }),
                tokio::fs::metadata(&chapter_path)
                    .await
                    .ok()
                    .map(|m| m.len()),
            )
            .await;
            storage.put_file(&key, &chapter_path, meta).await?;
            written_keys.push(key.clone());
            if first_key.is_empty() {
                first_key = key;
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

    if let Some(cover) = cover_path.as_ref() {
        if req.options.download_cover {
            let cover_key = sidecar_key(&storage_key, "jpg");
            let meta = sidecar_meta(
                object_asin_for(library, req).as_str(),
                &req.title,
                "image/jpeg",
                cover,
            )
            .await;
            if let Err(err) = storage.put_file(&cover_key, cover, meta).await {
                tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
            }
        }
    }

    if let Err(err) = tokio::fs::remove_dir_all(work_dir).await {
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

async fn store_plain_fetch(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    work_dir: &Path,
    plain: PlainFetch,
) -> Result<LiberateResult> {
    let want_mp3 = req.options.wants_mp3();
    let multi = plain.parts.len() > 1;
    let audible_overlay_possible = plain_source_has_audible_asin(library, req);

    // No-op output: keep store-delivered bytes (no remux/transcode).
    if req.options.is_noop_output() {
        if multi {
            return store_plain_parts(library, storage, req, plain).await;
        }
        let path = plain
            .m4b_path
            .clone()
            .or_else(|| plain.parts.first().map(|p| p.path.clone()))
            .ok_or_else(|| {
                LiberateError::Other(anyhow::anyhow!(
                    "source `{}` returned no audio for {}",
                    req.source,
                    req.asin
                ))
            })?;
        return store_plain_parts(
            library,
            storage,
            req,
            PlainFetch {
                parts: vec![libation_source::PlainAudioPart {
                    path,
                    title: None,
                    duration_ms: None,
                }],
                m4b_path: None,
                cover_path: plain.cover_path,
                chapters: plain.chapters,
            },
        )
        .await;
    }

    // Multi-part "split by chapter" without enrichment: store parts as-is.
    // When an Audible ASIN enrichment is available, package first so we can embed /
    // split by the literary chapter tree instead of track-boundary placeholders.
    if multi && req.options.wants_split_by_chapter() && !audible_overlay_possible {
        return store_plain_parts(library, storage, req, plain).await;
    }

    let mut chapters = plain.chapters.clone();
    let mut replace_chapters = false;
    let mut liberated_path = if let Some(m4b) = plain.m4b_path.clone() {
        m4b
    } else if plain.parts.is_empty() {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "source `{}` returned no audio for {}",
            req.source,
            req.asin
        )));
    } else if plain.parts.len() == 1 && want_mp3 && !audible_overlay_possible {
        plain.parts[0].path.clone()
    } else {
        // Package MP3/M4A part(s) into M4B (single-file M4B target, multi→single, or
        // Audible chapter overlay that needs a contiguous timeline before chapter split).
        let titles: Vec<String> = plain
            .parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                p.title
                    .clone()
                    .unwrap_or_else(|| format!("Chapter {}", i + 1))
            })
            .collect();
        let out = work_dir.join(format!("{}.m4b", status_key(req)));
        let (outcome, packaged_chapters) = package_m4b_from_mp3(PackageM4bRequest {
            parts: plain.parts.iter().map(|p| p.path.clone()).collect(),
            output: out,
            chapter_titles: titles,
        })
        .await?;
        if chapters.is_empty() {
            chapters = packaged_chapters;
        }
        outcome.output
    };

    // Store track markers (Chirp/Libro) are not literary chapters. When the row
    // has an enriched Audible ASIN, overlay Audible's chapter tree and shift
    // timestamps past Audible brand intro/outro (absent from plain audio).
    let mut catalog = PlainAudibleCatalog::default();
    let mut plain_chapter_tree: Option<Value> = None;
    if audible_overlay_possible {
        let plain_duration = probe_audio_duration_ms(&liberated_path);
        catalog = fetch_plain_audible_catalog(library, req, work_dir).await;
        if let Some((overlaid, tree)) =
            overlay_audible_chapters_for_plain(library, req, plain_duration).await
        {
            tracing::info!(
                id = %status_key(req),
                source = %req.source,
                audible_asin = ?resolve_book(library, req).and_then(|b| b.asin.clone()),
                chapters = overlaid.len(),
                plain_duration_ms = ?plain_duration,
                "overlaying Audible chapter tree onto plain audio"
            );
            // Local ±5s speech-band snap: cheap (decode only small windows),
            // places markers up to 2s before the spoken-title onset without
            // crossing prior vocal energy (helps with music beds too).
            let aligned =
                align_chapter_starts(&liberated_path, &overlaid, ChapterAlignOptions::default());
            let mut start_map = std::collections::HashMap::new();
            for ((_, old), (_, new)) in overlaid.iter().zip(aligned.iter()) {
                start_map.insert(*old, *new);
            }
            plain_chapter_tree = Some(apply_start_map_to_chapter_tree(&tree, &start_map));
            chapters = aligned;
            replace_chapters = true;
        }
    }

    let flat_chapters: Vec<FlatChapter> = chapters
        .iter()
        .map(|(title, start_ms)| FlatChapter {
            title: title.clone(),
            start_ms: *start_ms,
        })
        .collect();
    let will_split = req.options.wants_split_by_chapter() && flat_chapters.len() > 1;

    if want_mp3 && !will_split {
        let ext = liberated_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.eq_ignore_ascii_case("mp3") {
            let mp3_out = work_dir.join(format!("{}.mp3", status_key(req)));
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

    // Prefer Audible catalog cover when enrichment matched an ASIN.
    let cover_path = catalog.cover_path.clone().or(plain.cover_path.clone());
    if req.options.fixup_metadata && !will_split {
        let fixed = work_dir.join(format!("{}.fixed.{}", status_key(req), ext));
        match fixup_audiobook(build_fixup_request(
            library,
            req,
            liberated_path.clone(),
            fixed.clone(),
            cover_path.clone(),
            chapters,
            None,
            replace_chapters,
            &catalog,
        ))
        .await
        {
            Ok(outcome) => liberated_path = outcome.output,
            Err(err) => {
                tracing::warn!(
                    id = %status_key(req),
                    error = %err,
                    "metadata fixup failed; storing pre-fixup audio"
                );
            }
        }
    }

    let storage_key = if will_split {
        let total_ms = probe_audio_duration_ms(&liberated_path)
            .or_else(|| {
                flat_chapters
                    .last()
                    .map(|c| c.start_ms.saturating_add(600_000))
            })
            .unwrap_or(3_600_000);
        let split_dir = work_dir.join("chapters");
        let file_ctx = naming_ctx(library, req);
        let folder_ctx = folder_naming_ctx(library, req);
        let split_chapters = split_audio_by_chapters(
            &liberated_path,
            &split_dir,
            &flat_chapters,
            total_ms,
            &folder_ctx,
            &file_ctx,
            &req.options,
            "m4b",
        )
        .await?;
        let mut first_key = String::new();
        let mut written_keys = Vec::new();
        for (idx, ch) in split_chapters.into_iter().enumerate() {
            let mut chapter_path = ch.path;
            let mut key = ch.storage_key;
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
                key = key
                    .trim_end_matches(".m4b")
                    .trim_end_matches(".M4B")
                    .to_string()
                    + ".mp3";
            }
            if req.options.fixup_metadata {
                let fixed = chapter_path.with_extension(format!("fixed.{}", ext));
                let chapter_chapters = vec![(ch.title.clone(), 0u64)];
                match fixup_audiobook(build_fixup_request(
                    library,
                    req,
                    chapter_path.clone(),
                    fixed.clone(),
                    cover_path.clone(),
                    chapter_chapters,
                    Some(format!("{} — {}", req.title, ch.title)),
                    true,
                    &catalog,
                ))
                .await
                {
                    Ok(outcome) => chapter_path = outcome.output,
                    Err(err) => {
                        tracing::warn!(
                            id = %status_key(req),
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
                content_type_for_ext(if want_mp3 { "mp3" } else { "m4b" }),
                tokio::fs::metadata(&chapter_path)
                    .await
                    .ok()
                    .map(|m| m.len()),
            )
            .await;
            storage.put_file(&key, &chapter_path, meta).await?;
            written_keys.push(key.clone());
            if first_key.is_empty() {
                first_key = key;
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

    if let Some(cover) = cover_path.as_ref() {
        if req.options.download_cover {
            let cover_key = sidecar_key(&storage_key, "jpg");
            let meta = sidecar_meta(
                object_asin_for(library, req).as_str(),
                &req.title,
                "image/jpeg",
                cover,
            )
            .await;
            if let Err(err) = storage.put_file(&cover_key, cover, meta).await {
                tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
            }
        }
    }

    // Flat sidecars for players/tools that ignore embedded Nero/QuickTime chapters;
    // also persist the nested Audnexus tree (timestamp-adjusted) when available.
    store_flat_chapter_sidecars(
        storage,
        req,
        &storage_key,
        work_dir,
        &flat_chapters,
        object_asin_for(library, req).as_str(),
    )
    .await;
    if let Some(tree) = plain_chapter_tree.as_ref() {
        store_chapter_tree_sidecar(
            storage,
            req,
            &storage_key,
            work_dir,
            tree,
            object_asin_for(library, req).as_str(),
        )
        .await;
    }

    if let Err(err) = tokio::fs::remove_dir_all(work_dir).await {
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

async fn store_plain_parts(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    plain: PlainFetch,
) -> Result<LiberateResult> {
    let file_ctx = naming_ctx(library, req);
    let folder_ctx = folder_naming_ctx(library, req);
    let mut first_key = String::new();
    let mut written_keys = Vec::new();

    for (idx, part) in plain.parts.iter().enumerate() {
        let ext = part
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3")
            .to_string();
        let title = part
            .title
            .clone()
            .unwrap_or_else(|| format!("Part {}", idx + 1));
        let mut part_ctx = file_ctx.clone();
        part_ctx.title = format!("{} — {title}", req.title);
        part_ctx.asin = format!("{}-p{:03}", req.asin, idx + 1);
        part_ctx.chapter_number = Some(u32::try_from(idx + 1).unwrap_or(1));
        part_ctx.chapter_title = Some(title.clone());
        let templates = req.options.naming_templates();
        let storage_key = storage_key_with_contexts(
            &folder_ctx,
            &part_ctx,
            Some(templates.folder.as_str()),
            Some(templates.file.as_str()),
            &ext,
            &req.options.replacement_characters,
            req.options.path_limits,
        );
        let meta = object_meta_for(
            library,
            req,
            &title,
            content_type_for_ext(&ext),
            tokio::fs::metadata(&part.path).await.ok().map(|m| m.len()),
        )
        .await;
        storage.put_file(&storage_key, &part.path, meta).await?;
        written_keys.push(storage_key.clone());
        if first_key.is_empty() {
            first_key = storage_key;
        }
    }

    apply_storage_timestamps(storage, library, req, &written_keys).await;

    if let Some(cover) = plain.cover_path.as_ref() {
        if req.options.download_cover {
            let cover_key = sidecar_key(&first_key, "jpg");
            let meta = sidecar_meta(
                object_asin_for(library, req).as_str(),
                &req.title,
                "image/jpeg",
                cover,
            )
            .await;
            if let Err(err) = storage.put_file(&cover_key, cover, meta).await {
                tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
            }
        }
    }

    Ok(LiberateResult {
        asin: req.asin.clone(),
        storage_key: first_key,
        matched_existing: false,
    })
}

async fn run_audible_pipeline(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
) -> Result<LiberateResult> {
    // LiberateRequest.asin is a title lookup id (uuid / product_id / asin / isbn);
    // Audible APIs need the store product ASIN when that differs.
    let audible_asin = audible_asin_for(library, req);

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
            &audible_asin,
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
        || req.options.wants_chapter_json()
        || req.options.split_files_by_chapter
        || req.options.strip_audible_brand_audio;
    let chapter_info = if need_chapters {
        match fetch_chapter_info(
            &account_client.client,
            &account_client.marketplace,
            &audible_asin,
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
    let mut runtime_ms = chapter_info
        .as_ref()
        .and_then(runtime_length_ms_from_chapter_info);
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
            &audible_asin,
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

    let want_mp3 = req.options.wants_mp3();
    let will_split = req.options.wants_split_by_chapter() && flat_chapters.len() > 1;

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
        // Audible chapters are rebased/title-processed; always replace embedded
        // chpl/tracks so brand-trim alignment reaches the stored file.
        match fixup_audiobook(build_fixup_request(
            library,
            req,
            liberated_path.clone(),
            fixed.clone(),
            cover_path.clone(),
            chapters,
            None,
            !flat_chapters.is_empty(),
            &PlainAudibleCatalog::default(),
        ))
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
                match fixup_audiobook(build_fixup_request(
                    library,
                    req,
                    chapter_path.clone(),
                    fixed.clone(),
                    cover_path.clone(),
                    chapter_chapters,
                    Some(format!("{} — {}", req.title, ch.title)),
                    true,
                    &PlainAudibleCatalog::default(),
                ))
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

/// Persist nested `chapters.tree.json` (Audnexus layout with adjusted timestamps).
async fn store_chapter_tree_sidecar(
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    audio_key: &str,
    work_dir: &Path,
    tree: &Value,
    asin: &str,
) {
    if !req.options.chapter_json_tree() {
        return;
    }
    let json_path = work_dir.join(format!("{}.chapters.tree.json", req.asin));
    match tokio::fs::write(
        &json_path,
        serde_json::to_vec_pretty(tree).unwrap_or_default(),
    )
    .await
    {
        Ok(()) => {
            let key = sidecar_key(audio_key, "chapters.tree.json");
            let meta = sidecar_meta(asin, &req.title, "application/json", &json_path).await;
            if let Err(err) = storage.put_file(&key, &json_path, meta).await {
                tracing::warn!(
                    asin = %req.asin,
                    key = %key,
                    error = %err,
                    "tree chapter json store failed"
                );
            }
        }
        Err(err) => {
            tracing::warn!(asin = %req.asin, error = %err, "tree chapter json write failed");
        }
    }
}

/// Write flat `.cue` / `chapters.flat.json` sidecars from the embedded marker list.
async fn store_flat_chapter_sidecars(
    storage: &dyn StorageBackend,
    req: &LiberateRequest,
    audio_key: &str,
    work_dir: &Path,
    flat_chapters: &[FlatChapter],
    asin: &str,
) {
    if flat_chapters.is_empty() {
        return;
    }

    if req.options.create_cue {
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
                let meta = sidecar_meta(asin, &req.title, "application/x-cue", &cue_path).await;
                if let Err(err) = storage.put_file(&key, &cue_path, meta).await {
                    tracing::warn!(asin = %req.asin, key = %key, error = %err, "cue store failed");
                }
            }
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "cue write failed");
            }
        }
    }

    if req.options.chapter_json_flat() {
        let json_path = work_dir.join(format!("{}.chapters.flat.json", req.asin));
        let payload = serde_json::json!({
            "layout": "flat",
            "chapters": flat_chapters.iter().map(|c| {
                serde_json::json!({
                    "title": c.title,
                    "startOffsetMs": c.start_ms,
                })
            }).collect::<Vec<_>>(),
        });
        match tokio::fs::write(
            &json_path,
            serde_json::to_vec_pretty(&payload).unwrap_or_default(),
        )
        .await
        {
            Ok(()) => {
                let key = sidecar_key(audio_key, "chapters.flat.json");
                let meta = sidecar_meta(asin, &req.title, "application/json", &json_path).await;
                if let Err(err) = storage.put_file(&key, &json_path, meta).await {
                    tracing::warn!(
                        asin = %req.asin,
                        key = %key,
                        error = %err,
                        "flat chapter json store failed"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "flat chapter json write failed");
            }
        }
    }
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

    // Nero/QuickTime are embedded in the M4B; also emit flat cue/JSON sidecars
    // for players that prefer an external marker list over chapter trees.
    store_flat_chapter_sidecars(storage, req, audio_key, work_dir, flat_chapters, &req.asin).await;

    if req.options.chapter_json_tree() {
        if let Some(info) = chapter_info {
            let json_path = work_dir.join(format!("{}.chapters.tree.json", req.asin));
            match tokio::fs::write(
                &json_path,
                serde_json::to_vec_pretty(info).unwrap_or_default(),
            )
            .await
            {
                Ok(()) => {
                    let key = sidecar_key(audio_key, "chapters.tree.json");
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
    let book = resolve_book(library, req);
    let created = resolve_timestamp(req.options.creation_time, book.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, book.as_ref());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(object_asin_for(library, req)),
        title: Some(title.to_string()),
        creation_time: created.map(system_time_rfc3339),
        last_write_time: modified.map(system_time_rfc3339),
    }
}

/// Prefer enriched Audible ASIN for S3 object metadata when present; otherwise
/// the liberate product id (Audible ASIN or Libro ISBN).
fn object_asin_for(library: &LibraryStore, req: &LiberateRequest) -> String {
    resolve_book(library, req)
        .and_then(|b| b.audible_asin().map(str::to_string))
        .unwrap_or_else(|| req.asin.clone())
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

fn content_type_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "m4a" | "m4b" => "audio/mp4",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" | "oga" => "audio/ogg",
        "aaxc" | "aax" | "cenc" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Clone, Default)]
struct PlainAudibleCatalog {
    title: Option<String>,
    authors: Option<String>,
    narrators: Option<String>,
    series: Option<String>,
    series_index: Option<String>,
    subtitle: Option<String>,
    publisher: Option<String>,
    isbn: Option<String>,
    categories: Option<String>,
    year: Option<String>,
    description: Option<String>,
    language: Option<String>,
    cover_path: Option<PathBuf>,
}

fn plain_source_has_audible_asin(library: &LibraryStore, req: &LiberateRequest) -> bool {
    if req.source == SourceKind::Audible {
        return false;
    }
    resolve_book(library, req)
        .and_then(|b| b.audible_asin().map(|_| ()))
        .is_some()
}

/// Fetch Audnexus catalog extras for plain liberate (chapters fetched separately).
async fn fetch_plain_audible_catalog(
    library: &LibraryStore,
    req: &LiberateRequest,
    work_dir: &Path,
) -> PlainAudibleCatalog {
    let Some(book) = resolve_book(library, req) else {
        return PlainAudibleCatalog::default();
    };
    let Some(audible_asin) = book.audible_asin().map(str::to_string) else {
        return PlainAudibleCatalog::default();
    };
    let region = if book.marketplace.trim().is_empty() {
        "us"
    } else {
        book.marketplace.as_str()
    };
    let http = match libation_enrich::public_http_client() {
        Ok(http) => http,
        Err(err) => {
            tracing::warn!(error = %err, "Audnexus HTTP client init failed");
            return PlainAudibleCatalog::default();
        }
    };
    let item = match fetch_audnexus_book(&http, &audible_asin, region).await {
        Ok(Some(item)) => item,
        Ok(None) => return PlainAudibleCatalog::default(),
        Err(err) => {
            tracing::warn!(
                audible_asin = %audible_asin,
                error = %err,
                "Audnexus book fetch failed for plain liberate overlay"
            );
            return PlainAudibleCatalog::default();
        }
    };

    let str_field = |key: &str| -> Option<String> {
        item.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let join_people = |key: &str| -> Option<String> {
        let arr = item.get(key)?.as_array()?;
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            None
        } else {
            Some(names.join(", "))
        }
    };
    let series = item
        .get("seriesPrimary")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let series_index = item
        .get("seriesPrimary")
        .and_then(|s| s.get("position"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_f64().map(|n| n.to_string()))
        })
        .filter(|s| !s.is_empty());
    let categories = item
        .get("genres")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
                .filter(|n| !n.is_empty())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty());
    let year = str_field("releaseDate").and_then(|d| {
        if d.len() >= 4 {
            Some(d[..4].to_string())
        } else {
            None
        }
    });
    let description = str_field("description").or_else(|| str_field("summary"));
    let isbn = item
        .get("isbn")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .filter(|s| !s.is_empty());

    let mut cover_path = None;
    if let Some(url) = item
        .get("image")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) if !bytes.is_empty() => {
                    let path = work_dir.join(format!("{audible_asin}.audnexus-cover.jpg"));
                    if tokio::fs::write(&path, &bytes).await.is_ok() {
                        cover_path = Some(path);
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(error = %err, "Audnexus cover body read failed");
                }
            },
            Ok(resp) => {
                tracing::debug!(status = %resp.status(), "Audnexus cover HTTP non-success");
            }
            Err(err) => {
                tracing::debug!(error = %err, "Audnexus cover download failed");
            }
        }
    }

    PlainAudibleCatalog {
        title: str_field("title"),
        authors: join_people("authors"),
        narrators: join_people("narrators"),
        series,
        series_index,
        subtitle: str_field("subtitle"),
        publisher: str_field("publisherName"),
        isbn,
        categories,
        year,
        description,
        language: str_field("language"),
        cover_path,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_fixup_request(
    library: &LibraryStore,
    req: &LiberateRequest,
    input: PathBuf,
    output: PathBuf,
    cover: Option<PathBuf>,
    chapters: Vec<(String, u64)>,
    title_override: Option<String>,
    replace_chapters: bool,
    catalog: &PlainAudibleCatalog,
) -> FixupRequest {
    let book = resolve_book(library, req);
    let year = catalog.year.clone().or_else(|| {
        book.as_ref()
            .and_then(|b| b.published_at)
            .map(|dt| dt.format("%Y").to_string())
    });
    let asin = book.as_ref().and_then(|b| b.asin.clone());
    let isbn = catalog.isbn.clone().or_else(|| {
        book.as_ref().and_then(|b| b.isbn.clone()).or_else(|| {
            if req.source == SourceKind::LibroFm {
                Some(req.asin.clone())
            } else {
                None
            }
        })
    });
    let title = title_override
        .unwrap_or_else(|| catalog.title.clone().unwrap_or_else(|| req.title.clone()));
    FixupRequest {
        input,
        output,
        title,
        author: catalog.authors.clone().or_else(|| req.authors.clone()),
        narrator: catalog.narrators.clone().or_else(|| req.narrators.clone()),
        cover,
        chapters,
        replace_chapters,
        subtitle: catalog
            .subtitle
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.subtitle.clone())),
        publisher: catalog
            .publisher
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.publisher.clone())),
        year,
        genre: catalog
            .categories
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.categories.clone())),
        series: catalog
            .series
            .clone()
            .or_else(|| req.series.clone())
            .or_else(|| book.as_ref().and_then(|b| b.series.clone())),
        series_index: catalog
            .series_index
            .clone()
            .or_else(|| req.series_index.clone())
            .or_else(|| book.as_ref().and_then(|b| b.series_index.clone())),
        asin,
        isbn,
        description: catalog.description.clone(),
        language: catalog.language.clone(),
        tool: Some(libation_tool_tag()),
    }
}

async fn apply_storage_timestamps(
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &LiberateRequest,
    keys: &[String],
) {
    let book = resolve_book(library, req);
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

fn resolve_book(
    library: &LibraryStore,
    req: &LiberateRequest,
) -> Option<libation_library::BookRecord> {
    if let Some(uuid) = req.book_uuid.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(Some(b)) = library.get_book_by_uuid(uuid) {
            return Some(b);
        }
    }
    library.get_book(&req.asin, &req.account_id).ok().flatten()
}

/// When liberating plain audio that was enriched with an Audible ASIN, fetch
/// Audible's chapter tree (Audnexus, no login) and rebase starts for missing
/// brand intro/outro.
///
/// `plain_audio_duration_ms` is the probed duration of the plain file (no brand
/// segments). When Audnexus omits runtime, it reconstructs the Audible timeline
/// so outro chapters can still be trimmed.
async fn overlay_audible_chapters_for_plain(
    library: &LibraryStore,
    req: &LiberateRequest,
    plain_audio_duration_ms: Option<u64>,
) -> Option<(Vec<(String, u64)>, Value)> {
    let book = resolve_book(library, req)?;
    let audible_asin = book.audible_asin()?.to_string();
    let region = if book.marketplace.trim().is_empty() {
        "us"
    } else {
        book.marketplace.as_str()
    };
    let info = match fetch_public_chapter_info(&audible_asin, region).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            tracing::debug!(
                audible_asin = %audible_asin,
                "Audnexus returned no chapters for plain-audio overlay"
            );
            return None;
        }
        Err(err) => {
            tracing::warn!(
                audible_asin = %audible_asin,
                error = %err,
                "Audnexus chapter fetch failed for plain-audio overlay"
            );
            return None;
        }
    };
    let chapters = chapters_from_audible_info_for_plain_audio(
        &info,
        req.options.combine_nested_chapter_titles,
        req.options.merge_opening_and_end_credits,
        req.options.strip_unabridged,
        req.options.strip_audible_brand_audio,
        plain_audio_duration_ms,
    );
    if chapters.is_empty() {
        None
    } else {
        let tree = rebase_chapter_tree_for_plain_audio(&info, plain_audio_duration_ms);
        Some((chapters, tree))
    }
}

fn probe_audio_duration_ms(path: &Path) -> Option<u64> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "m4b" | "m4a" | "mp4" | "aaxc" | "aax") {
        if let Ok(mp4) = parse_mp4(path) {
            let ms = track_duration_ms(&mp4.audio);
            if ms > 0 {
                return Some(ms);
            }
        }
    }
    None
}

fn naming_ctx(library: &LibraryStore, req: &LiberateRequest) -> NamingContext {
    let book = resolve_book(library, req);
    NamingContext {
        asin: book
            .as_ref()
            .map(|b| b.asin_or_isbn().to_string())
            .unwrap_or_else(|| req.asin.clone()),
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
        year_published: book
            .as_ref()
            .and_then(|b| b.published_at)
            .map(|dt| dt.year()),
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

/// Resolve the Audible product ASIN for license/download APIs.
fn audible_asin_for(library: &LibraryStore, req: &LiberateRequest) -> String {
    resolve_book(library, req)
        .and_then(|b| b.audible_asin().map(str::to_string))
        .unwrap_or_else(|| req.asin.clone())
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
        asin: parent.asin_or_isbn().to_string(),
        title: parent.title.clone(),
        subtitle: parent.subtitle.clone(),
        authors: parent.authors.clone(),
        narrators: parent.narrators.clone(),
        series: parent.series.clone(),
        series_index: None,
        series_asin: parent.series_asin.clone(),
        year_published: parent.published_at.map(|dt| dt.year()),
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
    let templates = req.options.naming_templates();
    storage_key_with_contexts(
        &folder_naming_ctx(library, req),
        &naming_ctx(library, req),
        Some(templates.folder.as_str()),
        Some(templates.file.as_str()),
        ext,
        replacement_rules,
        req.options.path_limits,
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
        if let Some(book) = resolve_book(library, req) {
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
    let audible_asin = audible_asin_for(library, req);
    let pdf_path = work_dir.join(format!("{}.pdf", audible_asin));

    let Some(path) = libation_audible::download_companion_pdf(
        &account.client,
        &account.marketplace,
        &audible_asin,
        &pdf_path,
    )
    .await?
    else {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "no companion PDF available for {}",
            audible_asin
        )));
    };

    let meta = sidecar_meta(&audible_asin, &req.title, "application/pdf", &path).await;
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
