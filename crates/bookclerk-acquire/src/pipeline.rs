//! Acquire pipeline: fetch (plain) → package / metadata → storage.
//!
//! DRM decrypt happens inside content-source plugins; this crate never sees keys.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bookclerk_config::{FileTimestampMode, MultiDestinationMode, OutputBackendKind};
use bookclerk_enrich::{fetch_audnexus_book, fetch_public_chapter_info};
use bookclerk_library::{AcquireStatus, LibraryStore};
use bookclerk_media::{
    align_chapter_starts, bookclerk_tool_tag, encode_to_mp3, fixup_audiobook, package_m4b_from_mp3,
    parse_mp4, track_duration_ms, ChapterAlignOptions, FixupRequest, PackageM4bRequest,
};
use bookclerk_source::{ContentSource, DownloadOptions, FetchOptions, PlainFetch};
use bookclerk_storage::{ObjectMeta, StorageBackend};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cue::{
    apply_start_map_to_chapter_tree, chapters_from_catalog_info_for_plain_audio,
    rebase_chapter_tree_for_plain_audio, write_cue, FlatChapter,
};
use crate::destinations::{AcquireDestination, AcquireDestinations};
use crate::error::{AcquireError, Result};
use crate::naming::{
    audio_basename, chapter_storage_key_with_folder, sidecar_key, storage_key_with_contexts,
    NamingContext,
};
use crate::reconcile::{find_existing_for_request, StorageIndex};
use crate::split::split_audio_by_chapters;

/// Request to acquire a single title.
#[derive(Debug, Clone)]
pub struct AcquireRequest {
    /// Download product id (Audible ASIN / Libro ISBN).
    pub asin: String,
    /// Stable library UUID when known (preferred for status updates).
    pub book_uuid: Option<String>,
    /// Which store owns this title (plugin id: `audible`, `libro`, …).
    pub source: String,
    pub account_id: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub options: DownloadOptions,
    /// Root for auth files (`BOOKCLERK_FILES_DIR`).
    pub files_dir: PathBuf,
    /// Scratch directory for encrypted + decrypted temps.
    pub cache_dir: PathBuf,
    /// When true, download even if matching media already exists in storage.
    pub force: bool,
    /// When set, only write prepared audio to these destination kinds
    /// (`output.multi_destination = refetch_missing`).
    pub write_destinations: Option<Vec<OutputBackendKind>>,
}

/// Result after a successful acquire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireResult {
    pub asin: String,
    pub storage_key: String,
    #[serde(default)]
    pub written_keys: Vec<String>,
    /// True when an existing file was matched and no download ran.
    pub matched_existing: bool,
}

/// Run the acquire pipeline for one book.
pub async fn acquire_book(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    acquire_book_indexed(library, destinations, req, None, source).await
}

/// Acquire with an optional pre-built [`StorageIndex`] (avoids re-listing storage
/// when liberating many titles). On success, newly written keys are inserted into
/// the index so later books in the same batch can match them.
///
/// Fetch always goes through [`ContentSource::fetch_title`] (Plain only;
/// Audible decrypts Adrm/CENC inside the plugin before returning).
pub async fn acquire_book_indexed(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    mut req: AcquireRequest,
    mut index: Option<&mut StorageIndex>,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    req.options = destinations.primary_destination().options.clone();
    tracing::info!(
        asin = %req.asin,
        source = %req.source,
        title = %req.title,
        force = req.force,
        output = ?req.options.effective_output(),
        "acquire requested"
    );

    if req.options.wants_opus() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "output=opus is not implemented yet; use enriched_m4b, single_mp3, \
             split_mp3_by_chapter, or none"
        )));
    }
    if req.options.wants_split_by_size() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "output=split_mp3_by_size is not implemented yet (split_mp3_max_mb={}); \
             use split_mp3_by_chapter or single_mp3",
            req.options.split_mp3_max_mb
        )));
    }

    if !req.force && !req.options.overwrite_existing {
        match plan_existing_destinations(library, destinations, &req, index.as_deref()).await? {
            ExistingPlan::Skip { primary_key } => {
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — matched existing acquired media"
                );
                library
                    .set_acquire_status(
                        status_key(&req),
                        &req.account_id,
                        AcquireStatus::Acquired,
                        Some(&primary_key),
                        None,
                    )
                    .await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                });
            }
            ExistingPlan::SyncMissing {
                primary_key,
                source_kind,
                source_key,
                missing,
            } => {
                tracing::info!(
                    asin = %req.asin,
                    from = ?source_kind,
                    missing = missing.len(),
                    "syncing existing media to missing destinations (no store fetch)"
                );
                let written =
                    sync_missing_destinations(destinations, source_kind, &source_key, &missing)
                        .await?;
                if let Some(idx) = index.as_mut() {
                    for key in &written {
                        idx.insert_key(key.clone());
                    }
                }
                library
                    .set_acquire_status(
                        status_key(&req),
                        &req.account_id,
                        AcquireStatus::Acquired,
                        Some(&primary_key),
                        None,
                    )
                    .await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: written,
                    matched_existing: true,
                });
            }
            ExistingPlan::Fetch {
                only_kinds: Some(kinds),
            } => {
                tracing::info!(
                    asin = %req.asin,
                    destinations = ?kinds,
                    "re-fetching into missing destinations only"
                );
                req.write_destinations = Some(kinds);
            }
            ExistingPlan::Fetch { only_kinds: None } => {
                // Full acquire into every destination (refetch_all, or nothing present).
            }
        }
    }

    library
        .set_acquire_status(
            status_key(&req),
            &req.account_id,
            AcquireStatus::Queued,
            None,
            None,
        )
        .await?;

    match run_pipeline(library, destinations, &req, source).await {
        Ok(result) => {
            if let Some(idx) = index.as_mut() {
                idx.insert_key(result.storage_key.clone());
                for key in &result.written_keys {
                    idx.insert_key(key.clone());
                }
            }
            library
                .set_acquire_status(
                    status_key(&req),
                    &req.account_id,
                    AcquireStatus::Acquired,
                    Some(&result.storage_key),
                    None,
                )
                .await?;
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            let _ = library
                .set_acquire_status(
                    status_key(&req),
                    &req.account_id,
                    AcquireStatus::Error,
                    None,
                    Some(&message),
                )
                .await;
            Err(err)
        }
    }
}

fn status_key(req: &AcquireRequest) -> &str {
    req.book_uuid.as_deref().unwrap_or(&req.asin)
}

#[derive(Debug)]
enum ExistingPlan {
    /// Every destination already has the title — skip acquire.
    Skip { primary_key: String },
    /// Copy from a present destination into missing ones (no store fetch).
    SyncMissing {
        primary_key: String,
        source_kind: OutputBackendKind,
        source_key: String,
        missing: Vec<(OutputBackendKind, String)>,
    },
    /// Run the full acquire pipeline (`only_kinds` limits writes when set).
    Fetch {
        only_kinds: Option<Vec<OutputBackendKind>>,
    },
}

async fn plan_existing_destinations(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    index: Option<&StorageIndex>,
) -> Result<ExistingPlan> {
    if destinations.len() == 1 {
        let dest = destinations.primary_destination();
        let dest_req = request_for_destination(req, dest);
        let owned_index;
        let lookup = match index {
            Some(idx) => idx,
            None => {
                owned_index = StorageIndex::from_storage(dest.backend.as_ref()).await?;
                &owned_index
            }
        };
        return Ok(
            match find_existing_for_request(lookup, library, &dest_req).await {
                Some(primary_key) => ExistingPlan::Skip { primary_key },
                None => ExistingPlan::Fetch { only_kinds: None },
            },
        );
    }

    let mut present: Vec<(OutputBackendKind, String)> = Vec::new();
    let mut missing_kinds: Vec<OutputBackendKind> = Vec::new();
    let mut primary_key = None;

    for dest in &destinations.items {
        let dest_req = request_for_destination(req, dest);
        let dest_index = StorageIndex::from_storage(dest.backend.as_ref()).await?;
        if let Some(key) = find_existing_for_request(&dest_index, library, &dest_req).await {
            if dest.kind == destinations.primary {
                primary_key = Some(key.clone());
            }
            present.push((dest.kind, key));
        } else {
            missing_kinds.push(dest.kind);
        }
    }

    let ext = present
        .first()
        .and_then(|(_, k)| k.rsplit_once('.').map(|(_, e)| e))
        .unwrap_or("m4b");
    let mut missing: Vec<(OutputBackendKind, String)> = Vec::new();
    for kind in missing_kinds {
        let Some(dest) = destinations.destination(kind) else {
            continue;
        };
        let dest_req = request_for_destination(req, dest);
        missing.push((kind, planned_storage_key_for(library, &dest_req, ext).await));
    }

    if missing.is_empty() {
        let primary_key = primary_key
            .or_else(|| present.first().map(|(_, k)| k.clone()))
            .unwrap_or_default();
        return Ok(ExistingPlan::Skip { primary_key });
    }
    if present.is_empty() {
        return Ok(ExistingPlan::Fetch { only_kinds: None });
    }

    let primary_key = primary_key
        .clone()
        .or_else(|| present.first().map(|(_, k)| k.clone()))
        .unwrap_or_else(|| missing[0].1.clone());

    match destinations.multi_destination {
        MultiDestinationMode::SyncMissing => {
            let (source_kind, source_key) = present[0].clone();
            Ok(ExistingPlan::SyncMissing {
                primary_key,
                source_kind,
                source_key,
                missing,
            })
        }
        MultiDestinationMode::RefetchMissing => Ok(ExistingPlan::Fetch {
            only_kinds: Some(missing.into_iter().map(|(k, _)| k).collect()),
        }),
        MultiDestinationMode::RefetchAll => Ok(ExistingPlan::Fetch { only_kinds: None }),
    }
}

const DEST_WRITE_ATTEMPTS: u32 = 3;

async fn sync_missing_destinations(
    destinations: &AcquireDestinations,
    source_kind: OutputBackendKind,
    source_key: &str,
    missing: &[(OutputBackendKind, String)],
) -> Result<Vec<String>> {
    let source = destinations.destination(source_kind).ok_or_else(|| {
        AcquireError::Other(anyhow::anyhow!(
            "sync source destination {:?} is not configured",
            source_kind
        ))
    })?;
    let bytes = source.backend.get(source_key).await?;
    let probe = source.backend.probe(source_key).await.unwrap_or_default();
    let mut meta = probe.meta;
    if meta.content_length.is_none() {
        meta.content_length = Some(bytes.len() as u64);
    }
    if meta.content_type.is_none() {
        meta.content_type = probe.content_type.or_else(|| {
            source_key
                .rsplit_once('.')
                .map(|(_, ext)| content_type_for_ext(ext).to_string())
        });
    }

    let mut written = Vec::new();
    for (kind, target_key) in missing {
        let dest = destinations.destination(*kind).ok_or_else(|| {
            AcquireError::Other(anyhow::anyhow!(
                "sync target destination {:?} is not configured",
                kind
            ))
        })?;
        let mut last_err = None;
        for attempt in 1..=DEST_WRITE_ATTEMPTS {
            match dest
                .backend
                .put(target_key, bytes.clone(), meta.clone())
                .await
            {
                Ok(()) => {
                    written.push(target_key.clone());
                    last_err = None;
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        destination = ?kind,
                        key = %target_key,
                        attempt,
                        %err,
                        "destination sync write failed; retrying"
                    );
                    last_err = Some(err);
                }
            }
        }
        if let Some(err) = last_err {
            return Err(AcquireError::Storage(err));
        }
    }
    Ok(written)
}

#[derive(Debug, Clone, Copy)]
enum AudioKeyPlan {
    Single,
    SplitChapters,
    PlainParts,
}

#[derive(Debug, Clone)]
struct PreparedAudioFile {
    path: PathBuf,
    title: String,
    ext: String,
}

#[derive(Debug, Clone)]
struct DestinationStoredKey {
    kind: OutputBackendKind,
    key: String,
}

#[derive(Debug, Clone)]
struct StoredKeys {
    primary_key: String,
    keys: Vec<DestinationStoredKey>,
}

impl StoredKeys {
    fn all_keys(&self) -> Vec<String> {
        self.keys.iter().map(|stored| stored.key.clone()).collect()
    }
}

fn request_for_destination(
    req: &AcquireRequest,
    destination: &AcquireDestination,
) -> AcquireRequest {
    let mut dest_req = req.clone();
    dest_req.options = destination.options.clone();
    dest_req
}

async fn store_prepared_audio_files(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    files: &[PreparedAudioFile],
    plan: AudioKeyPlan,
) -> Result<StoredKeys> {
    if files.is_empty() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "no audio files were prepared for storage"
        )));
    }

    let mut primary_key = None;
    let mut keys = Vec::new();
    for dest in &destinations.items {
        if let Some(filter) = req.write_destinations.as_deref() {
            if !filter.is_empty() && !filter.contains(&dest.kind) {
                continue;
            }
        }
        let dest_req = request_for_destination(req, dest);
        let mut last_err = None;
        let mut stored = None;
        for attempt in 1..=DEST_WRITE_ATTEMPTS {
            match write_prepared_files_to_destination(library, dest, &dest_req, files, plan).await {
                Ok((first_key, written_keys)) => {
                    apply_storage_timestamps(
                        dest.backend.as_ref(),
                        library,
                        &dest_req,
                        &written_keys,
                    )
                    .await;
                    if dest.kind == destinations.primary {
                        primary_key = Some(first_key.clone());
                    }
                    stored = Some(DestinationStoredKey {
                        kind: dest.kind,
                        key: first_key,
                    });
                    last_err = None;
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        destination = ?dest.kind,
                        asin = %req.asin,
                        attempt,
                        %err,
                        "destination write failed; retrying"
                    );
                    last_err = Some(err);
                }
            }
        }
        if let Some(err) = last_err {
            // Retain successful writes on other destinations; fail this book.
            return Err(err);
        }
        if let Some(stored) = stored {
            keys.push(stored);
        }
    }

    let primary_key = primary_key
        .or_else(|| keys.first().map(|stored| stored.key.clone()))
        .unwrap_or_default();
    Ok(StoredKeys { primary_key, keys })
}

async fn write_prepared_files_to_destination(
    library: &LibraryStore,
    dest: &AcquireDestination,
    dest_req: &AcquireRequest,
    files: &[PreparedAudioFile],
    plan: AudioKeyPlan,
) -> Result<(String, Vec<String>)> {
    let mut first_key = None;
    let mut written_keys = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let key = planned_key_for_prepared_file(library, dest_req, file, idx, plan).await;
        let meta = object_meta_for(
            library,
            dest_req,
            &file.title,
            content_type_for_ext(&file.ext),
            tokio::fs::metadata(&file.path).await.ok().map(|m| m.len()),
        )
        .await;
        dest.backend.put_file(&key, &file.path, meta).await?;
        if first_key.is_none() {
            first_key = Some(key.clone());
        }
        written_keys.push(key);
    }
    Ok((first_key.unwrap_or_default(), written_keys))
}

async fn planned_key_for_prepared_file(
    library: &LibraryStore,
    req: &AcquireRequest,
    file: &PreparedAudioFile,
    idx: usize,
    plan: AudioKeyPlan,
) -> String {
    match plan {
        AudioKeyPlan::Single => planned_storage_key_for(library, req, &file.ext).await,
        AudioKeyPlan::SplitChapters => planned_chapter_storage_key(library, req, idx, file).await,
        AudioKeyPlan::PlainParts => planned_plain_part_storage_key(library, req, idx, file).await,
    }
}

async fn planned_chapter_storage_key(
    library: &LibraryStore,
    req: &AcquireRequest,
    idx: usize,
    file: &PreparedAudioFile,
) -> String {
    let templates = req.options.naming_templates();
    let folder_ctx = folder_naming_ctx(library, req).await;
    let file_ctx = naming_ctx(library, req).await;
    chapter_storage_key_with_folder(
        &folder_ctx,
        &file_ctx,
        Some(templates.folder.as_str()),
        Some(templates.chapter_file.as_str()),
        &req.options.replacement_characters,
        idx + 1,
        &file.title,
        &file.ext,
        req.options.path_limits,
    )
}

async fn planned_plain_part_storage_key(
    library: &LibraryStore,
    req: &AcquireRequest,
    idx: usize,
    file: &PreparedAudioFile,
) -> String {
    let file_ctx = naming_ctx(library, req).await;
    let folder_ctx = folder_naming_ctx(library, req).await;
    let mut part_ctx = file_ctx;
    part_ctx.title = format!("{} — {}", req.title, file.title);
    part_ctx.asin = format!("{}-p{:03}", req.asin, idx + 1);
    part_ctx.chapter_number = Some(u32::try_from(idx + 1).unwrap_or(1));
    part_ctx.chapter_title = Some(file.title.clone());
    let templates = req.options.naming_templates();
    storage_key_with_contexts(
        &folder_ctx,
        &part_ctx,
        Some(templates.folder.as_str()),
        Some(templates.file.as_str()),
        &file.ext,
        &req.options.replacement_characters,
        req.options.path_limits,
    )
}

async fn run_pipeline(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    library
        .set_acquire_status(
            status_key(req),
            &req.account_id,
            AcquireStatus::Downloading,
            None,
            None,
        )
        .await?;

    run_source_pipeline(library, destinations, req, source).await
}

async fn run_source_pipeline(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let work_dir = req.cache_dir.join("acquire").join(status_key(req));
    tokio::fs::create_dir_all(&work_dir).await?;

    let scope = library.scope(source.id());
    let fetch = source
        .fetch_title(
            &scope,
            &req.account_id,
            &req.asin,
            &FetchOptions {
                download: req.options.clone(),
                cache_dir: work_dir.clone(),
                files_dir: req.files_dir.clone(),
            },
        )
        .await?;

    store_plain_fetch(library, destinations, req, &work_dir, fetch).await
}

async fn store_plain_fetch(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    plain: PlainFetch,
) -> Result<AcquireResult> {
    let want_mp3 = req.options.wants_mp3();
    let multi = plain.parts.len() > 1;
    let audible_overlay_possible = plain_source_has_audible_asin(library, req).await;

    // No-op output: keep store-delivered bytes (no remux/transcode).
    if req.options.is_noop_output() {
        if multi {
            return store_plain_parts(library, destinations, req, work_dir, plain).await;
        }
        let path = plain
            .m4b_path
            .clone()
            .or_else(|| plain.parts.first().map(|p| p.path.clone()))
            .ok_or_else(|| {
                AcquireError::Other(anyhow::anyhow!(
                    "source `{}` returned no audio for {}",
                    req.source,
                    req.asin
                ))
            })?;
        return store_plain_parts(
            library,
            destinations,
            req,
            work_dir,
            PlainFetch {
                parts: vec![bookclerk_source::PlainAudioPart {
                    path,
                    title: None,
                    duration_ms: None,
                }],
                m4b_path: None,
                cover_path: plain.cover_path,
                chapters: plain.chapters,
                pdf_url: plain.pdf_url,
            },
        )
        .await;
    }

    // Multi-part "split by chapter" without enrichment: store parts as-is.
    // When an Audible ASIN enrichment is available, package first so we can embed /
    // split by the literary chapter tree instead of track-boundary placeholders.
    if multi && req.options.wants_split_by_chapter() && !audible_overlay_possible {
        return store_plain_parts(library, destinations, req, work_dir, plain).await;
    }

    let mut chapters = plain.chapters.clone();
    let mut replace_chapters = false;
    let mut acquired_path = if let Some(m4b) = plain.m4b_path.clone() {
        m4b
    } else if plain.parts.is_empty() {
        return Err(AcquireError::Other(anyhow::anyhow!(
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
        let plain_duration = probe_audio_duration_ms(&acquired_path);
        catalog = fetch_plain_catalog_overlay(library, req, work_dir).await;
        if let Some((overlaid, tree)) =
            overlay_audible_chapters_for_plain(library, req, plain_duration).await
        {
            let audible_asin = resolve_book(library, req)
                .await
                .and_then(|b| b.asin.clone());
            tracing::info!(
                id = %status_key(req),
                source = %req.source,
                audible_asin = ?audible_asin,
                chapters = overlaid.len(),
                plain_duration_ms = ?plain_duration,
                "overlaying Audible chapter tree onto plain audio"
            );
            // Local ±5s speech-band snap: cheap (decode only small windows),
            // places markers up to 2s before the spoken-title onset without
            // crossing prior vocal energy (helps with music beds too).
            let aligned =
                align_chapter_starts(&acquired_path, &overlaid, ChapterAlignOptions::default());
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
        let ext = acquired_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.eq_ignore_ascii_case("mp3") {
            let mp3_out = work_dir.join(format!("{}.mp3", status_key(req)));
            encode_to_mp3(
                &acquired_path,
                &mp3_out,
                &req.options.lame,
                req.options.max_sample_rate,
            )
            .await?;
            acquired_path = mp3_out;
        }
    }

    let ext = if will_split && want_mp3 {
        "mp3".to_string()
    } else {
        acquired_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_else(|| fallback_audio_ext(&req.options))
            .to_string()
    };

    // Prefer Audible catalog cover when enrichment matched an ASIN.
    let cover_path = catalog.cover_path.clone().or(plain.cover_path.clone());
    if req.options.fixup_metadata && !will_split {
        let fixed = work_dir.join(format!("{}.fixed.{}", status_key(req), ext));
        match fixup_audiobook(
            build_fixup_request(
                library,
                req,
                acquired_path.clone(),
                fixed.clone(),
                cover_path.clone(),
                chapters,
                None,
                replace_chapters,
                &catalog,
            )
            .await,
        )
        .await
        {
            Ok(outcome) => acquired_path = outcome.output,
            Err(err) => {
                tracing::warn!(
                    id = %status_key(req),
                    error = %err,
                    "metadata fixup failed; storing pre-fixup audio"
                );
            }
        }
    }

    let stored_keys = if will_split {
        let total_ms = probe_audio_duration_ms(&acquired_path)
            .or_else(|| {
                flat_chapters
                    .last()
                    .map(|c| c.start_ms.saturating_add(600_000))
            })
            .unwrap_or(3_600_000);
        let split_dir = work_dir.join("chapters");
        let file_ctx = naming_ctx(library, req).await;
        let folder_ctx = folder_naming_ctx(library, req).await;
        let split_chapters = split_audio_by_chapters(
            &acquired_path,
            &split_dir,
            &flat_chapters,
            total_ms,
            &folder_ctx,
            &file_ctx,
            &req.options,
            "m4b",
        )
        .await?;
        let mut prepared = Vec::new();
        for (idx, ch) in split_chapters.into_iter().enumerate() {
            let mut chapter_path = ch.path;
            let mut chapter_ext = "m4b".to_string();
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
                chapter_ext = "mp3".to_string();
            }
            if req.options.fixup_metadata {
                let fixed = chapter_path.with_extension(format!("fixed.{}", ext));
                let chapter_chapters = vec![(ch.title.clone(), 0u64)];
                match fixup_audiobook(
                    build_fixup_request(
                        library,
                        req,
                        chapter_path.clone(),
                        fixed.clone(),
                        cover_path.clone(),
                        chapter_chapters,
                        Some(format!("{} — {}", req.title, ch.title)),
                        true,
                        &catalog,
                    )
                    .await,
                )
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
            prepared.push(PreparedAudioFile {
                path: chapter_path,
                title: ch.title,
                ext: chapter_ext,
            });
        }
        store_prepared_audio_files(
            library,
            destinations,
            req,
            &prepared,
            AudioKeyPlan::SplitChapters,
        )
        .await?
    } else {
        let prepared = [PreparedAudioFile {
            path: acquired_path.clone(),
            title: req.title.clone(),
            ext: ext.clone(),
        }];
        store_prepared_audio_files(library, destinations, req, &prepared, AudioKeyPlan::Single)
            .await?
    };

    if let Some(cover) = cover_path.as_ref() {
        for stored in &stored_keys.keys {
            if let Some(dest) = destinations.destination(stored.kind) {
                let dest_req = request_for_destination(req, dest);
                if dest_req.options.download_cover {
                    let cover_key = sidecar_key(&stored.key, "jpg");
                    let asin_str = object_asin_for(library, &dest_req).await;
                    let meta =
                        sidecar_meta(asin_str.as_str(), &dest_req.title, "image/jpeg", cover).await;
                    if let Err(err) = dest.backend.put_file(&cover_key, cover, meta).await {
                        tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
                    }
                }
            }
        }
    }

    // Flat sidecars for players/tools that ignore embedded Nero/QuickTime chapters;
    // also persist the nested Audnexus tree (timestamp-adjusted) when available.
    for stored in &stored_keys.keys {
        if let Some(dest) = destinations.destination(stored.kind) {
            let dest_req = request_for_destination(req, dest);
            let asin_str = object_asin_for(library, &dest_req).await;
            store_flat_chapter_sidecars(
                dest.backend.as_ref(),
                &dest_req,
                &stored.key,
                work_dir,
                &flat_chapters,
                asin_str.as_str(),
            )
            .await;
            if let Some(tree) = plain_chapter_tree.as_ref() {
                let asin_str2 = object_asin_for(library, &dest_req).await;
                store_chapter_tree_sidecar(
                    dest.backend.as_ref(),
                    &dest_req,
                    &stored.key,
                    work_dir,
                    tree,
                    asin_str2.as_str(),
                )
                .await;
            }
        }
    }

    let mut written_keys = stored_keys.all_keys();
    if let Ok(pdf_keys) = store_companion_pdf_sidecars(
        library,
        destinations,
        req,
        work_dir,
        plain.pdf_url.as_deref(),
    )
    .await
    {
        written_keys.extend(pdf_keys);
    }

    if let Err(err) = tokio::fs::remove_dir_all(work_dir).await {
        tracing::warn!(
            path = %work_dir.display(),
            error = %err,
            "failed to clean acquire cache dir"
        );
    }

    Ok(AcquireResult {
        asin: req.asin.clone(),
        storage_key: stored_keys.primary_key.clone(),
        written_keys,
        matched_existing: false,
    })
}

async fn store_plain_parts(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    plain: PlainFetch,
) -> Result<AcquireResult> {
    let mut prepared = Vec::new();
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
        prepared.push(PreparedAudioFile {
            path: part.path.clone(),
            title,
            ext,
        });
    }

    let stored_keys = store_prepared_audio_files(
        library,
        destinations,
        req,
        &prepared,
        AudioKeyPlan::PlainParts,
    )
    .await?;

    if let Some(cover) = plain.cover_path.as_ref() {
        for stored in &stored_keys.keys {
            if let Some(dest) = destinations.destination(stored.kind) {
                let dest_req = request_for_destination(req, dest);
                if dest_req.options.download_cover {
                    let cover_key = sidecar_key(&stored.key, "jpg");
                    let asin_str = object_asin_for(library, &dest_req).await;
                    let meta =
                        sidecar_meta(asin_str.as_str(), &dest_req.title, "image/jpeg", cover).await;
                    if let Err(err) = dest.backend.put_file(&cover_key, cover, meta).await {
                        tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
                    }
                }
            }
        }
    }

    let mut written_keys = stored_keys.all_keys();
    if let Ok(pdf_keys) = store_companion_pdf_sidecars(
        library,
        destinations,
        req,
        work_dir,
        plain.pdf_url.as_deref(),
    )
    .await
    {
        written_keys.extend(pdf_keys);
    }

    Ok(AcquireResult {
        asin: req.asin.clone(),
        storage_key: stored_keys.primary_key.clone(),
        written_keys,
        matched_existing: false,
    })
}

/// Download + store companion PDF sidecars when `output.download_pdf` is enabled.
///
/// Soft-fails (logs + returns `Ok(empty)`) when the URL is missing or HTTP fails so
/// audio acquire still succeeds. Skips when a PDF is already marked acquired unless
/// `req.force`. Uses the planned single-book audio key per destination (not every
/// chapter file when splitting).
async fn store_companion_pdf_sidecars(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    pdf_url: Option<&str>,
) -> Result<Vec<String>> {
    if !req.options.download_pdf {
        return Ok(Vec::new());
    }
    let Some(pdf_url) = pdf_url.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };

    if !req.force {
        if let Some(book) = resolve_book(library, req).await {
            if book.pdf_status == AcquireStatus::Acquired && book.pdf_storage_key.is_some() {
                return Ok(Vec::new());
            }
        }
    }

    let pdf_path = work_dir.join(format!("{}.pdf", req.asin));
    let client = reqwest::Client::new();
    let bytes = match client.get(pdf_url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => match ok.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(id = %status_key(req), error = %err, "PDF download body failed");
                    return Ok(Vec::new());
                }
            },
            Err(err) => {
                tracing::warn!(id = %status_key(req), error = %err, "PDF download failed");
                return Ok(Vec::new());
            }
        },
        Err(err) => {
            tracing::warn!(id = %status_key(req), error = %err, "PDF download failed");
            return Ok(Vec::new());
        }
    };
    if let Err(err) = tokio::fs::write(&pdf_path, &bytes).await {
        tracing::warn!(id = %status_key(req), error = %err, "PDF write failed");
        return Ok(Vec::new());
    }

    let mut primary_pdf_key = None;
    let mut written = Vec::new();
    for dest in &destinations.items {
        let dest_req = request_for_destination(req, dest);
        let audio_key = planned_storage_key(library, &dest_req).await;
        let pdf_key = sidecar_key(&audio_key, "pdf");
        let asin_str = object_asin_for(library, &dest_req).await;
        let meta = sidecar_meta(&asin_str, &dest_req.title, "application/pdf", &pdf_path).await;
        match dest.backend.put_file(&pdf_key, &pdf_path, meta).await {
            Ok(()) => {
                if dest.kind == destinations.primary {
                    primary_pdf_key = Some(pdf_key.clone());
                }
                written.push(pdf_key);
            }
            Err(err) => {
                tracing::warn!(id = %status_key(req), error = %err, "PDF store failed");
            }
        }
    }

    if let Some(pdf_key) = primary_pdf_key.or_else(|| written.first().cloned()) {
        if let Err(err) = library
            .set_pdf_status(
                &req.asin,
                &req.account_id,
                AcquireStatus::Acquired,
                Some(&pdf_key),
            )
            .await
        {
            tracing::warn!(id = %status_key(req), error = %err, "PDF status update failed");
        }
    }

    Ok(written)
}

/// Persist nested `chapters.tree.json` (Audnexus layout with adjusted timestamps).
async fn store_chapter_tree_sidecar(
    storage: &dyn StorageBackend,
    req: &AcquireRequest,
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
    req: &AcquireRequest,
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

async fn object_meta_for(
    library: &LibraryStore,
    req: &AcquireRequest,
    title: &str,
    content_type: &str,
    content_length: Option<u64>,
) -> ObjectMeta {
    let book = resolve_book(library, req).await;
    let created = resolve_timestamp(req.options.creation_time, book.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, book.as_ref());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(object_asin_for(library, req).await),
        title: Some(title.to_string()),
        creation_time: created.map(system_time_rfc3339),
        last_write_time: modified.map(system_time_rfc3339),
    }
}

/// Prefer enriched Audible ASIN for S3 object metadata when present; otherwise
/// the acquire product id (Audible ASIN or Libro ISBN).
async fn object_asin_for(library: &LibraryStore, req: &AcquireRequest) -> String {
    resolve_book(library, req)
        .await
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

fn fallback_audio_ext(options: &DownloadOptions) -> &'static str {
    if options.wants_mp3() {
        "mp3"
    } else if options.wants_opus() {
        "opus"
    } else {
        "m4b"
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

async fn plain_source_has_audible_asin(library: &LibraryStore, req: &AcquireRequest) -> bool {
    resolve_book(library, req)
        .await
        .and_then(|b| {
            let asin = b.asin.as_deref()?;
            // Audible-native rows set asin == product_id; enrichment ASINs differ.
            if asin == b.product_id.as_str() {
                None
            } else {
                Some(())
            }
        })
        .is_some()
}

/// Fetch Audnexus catalog extras for plain acquire (chapters fetched separately).
async fn fetch_plain_catalog_overlay(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
) -> PlainAudibleCatalog {
    let Some(book) = resolve_book(library, req).await else {
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
    let http = match bookclerk_enrich::public_http_client() {
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
                "Audnexus book fetch failed for plain acquire overlay"
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
async fn build_fixup_request(
    library: &LibraryStore,
    req: &AcquireRequest,
    input: PathBuf,
    output: PathBuf,
    cover: Option<PathBuf>,
    chapters: Vec<(String, u64)>,
    title_override: Option<String>,
    replace_chapters: bool,
    catalog: &PlainAudibleCatalog,
) -> FixupRequest {
    let book = resolve_book(library, req).await;
    let year = catalog.year.clone().or_else(|| {
        book.as_ref()
            .and_then(|b| b.published_at)
            .map(|dt| dt.format("%Y").to_string())
    });
    let asin = book.as_ref().and_then(|b| b.asin.clone());
    let isbn = catalog.isbn.clone().or_else(|| {
        book.as_ref().and_then(|b| {
            b.isbn.clone().or_else(|| {
                // product_id is the store-native id; use it as ISBN when it is not the ASIN.
                if b.asin.as_deref() != Some(b.product_id.as_str()) {
                    Some(b.product_id.clone())
                } else {
                    None
                }
            })
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
        tool: Some(bookclerk_tool_tag()),
    }
}

async fn apply_storage_timestamps(
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &AcquireRequest,
    keys: &[String],
) {
    let book = resolve_book(library, req).await;
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
    book: Option<&bookclerk_library::BookRecord>,
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

async fn resolve_book(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Option<bookclerk_library::BookRecord> {
    if let Some(uuid) = req.book_uuid.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(Some(b)) = library.get_book_by_uuid(uuid).await {
            return Some(b);
        }
    }
    library
        .get_book(&req.asin, &req.account_id)
        .await
        .ok()
        .flatten()
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
    req: &AcquireRequest,
    plain_audio_duration_ms: Option<u64>,
) -> Option<(Vec<(String, u64)>, Value)> {
    let book = resolve_book(library, req).await?;
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
    let chapters = chapters_from_catalog_info_for_plain_audio(
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

async fn naming_ctx(library: &LibraryStore, req: &AcquireRequest) -> NamingContext {
    let book = resolve_book(library, req).await;
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

/// Folder naming context: when saving podcasts to the parent folder, evaluate
/// the folder template against the podcast parent (classic Libation behavior).
async fn folder_naming_ctx(library: &LibraryStore, req: &AcquireRequest) -> NamingContext {
    let episode = naming_ctx(library, req).await;
    if !req.options.save_podcasts_to_parent_folder {
        return episode;
    }
    let kind = episode.content_kind.as_deref().unwrap_or("");
    if !bookclerk_library::is_episode(kind) {
        return episode;
    }
    let Some(parent_asin) = episode.series_asin.as_deref() else {
        return episode;
    };
    let Ok(Some(parent)) = library.get_book(parent_asin, &req.account_id).await else {
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
pub async fn planned_storage_key_for(
    library: &LibraryStore,
    req: &AcquireRequest,
    ext: &str,
) -> String {
    planned_storage_key_with_rules(library, req, ext, &req.options.replacement_characters).await
}

/// Like [`planned_storage_key_for`] but with an explicit replacement-rule set
/// (used by reconcile to probe wildcard patterns across sanitization profiles).
pub async fn planned_storage_key_with_rules(
    library: &LibraryStore,
    req: &AcquireRequest,
    ext: &str,
    replacement_rules: &[bookclerk_config::ReplacementRule],
) -> String {
    let templates = req.options.naming_templates();
    let folder_ctx = folder_naming_ctx(library, req).await;
    let file_ctx = naming_ctx(library, req).await;
    storage_key_with_contexts(
        &folder_ctx,
        &file_ctx,
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
/// `save_podcasts_to_parent_folder` the same way as a real acquire.
pub async fn planned_storage_key(library: &LibraryStore, req: &AcquireRequest) -> String {
    planned_storage_key_for(library, req, fallback_audio_ext(&req.options)).await
}

/// Download and store companion PDF only (classic `acquire --pdf`).
///
/// Fetches via [`ContentSource::fetch_title`]; requires `PlainFetch.pdf_url`, then HTTP GETs that URL.
pub async fn acquire_pdf_only(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let primary_req = request_for_destination(req, destinations.primary_destination());

    if !primary_req.force {
        if let Some(book) = resolve_book(library, &primary_req).await {
            if book.pdf_status == AcquireStatus::Acquired {
                if let Some(key) = book.pdf_storage_key {
                    return Ok(AcquireResult {
                        asin: primary_req.asin.clone(),
                        storage_key: key,
                        written_keys: Vec::new(),
                        matched_existing: true,
                    });
                }
            }
        }
    }

    let work_dir = primary_req
        .cache_dir
        .join("acquire-pdf")
        .join(&primary_req.asin);
    tokio::fs::create_dir_all(&work_dir).await?;

    let mut download_opts = primary_req.options.clone();
    download_opts.download_pdf = true;

    let scope = library.scope(source.id());
    let fetch = source
        .fetch_title(
            &scope,
            &primary_req.account_id,
            &primary_req.asin,
            &FetchOptions {
                download: download_opts,
                cache_dir: work_dir.clone(),
                files_dir: primary_req.files_dir.clone(),
            },
        )
        .await?;

    let pdf_url = fetch.pdf_url;
    let Some(pdf_url) = pdf_url else {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "no companion PDF available for {}",
            primary_req.asin
        )));
    };

    let pdf_path = work_dir.join(format!("{}.pdf", primary_req.asin));
    let client = reqwest::Client::new();
    let bytes = client
        .get(&pdf_url)
        .send()
        .await
        .map_err(|e| AcquireError::Other(anyhow::anyhow!("PDF download failed: {e}")))?
        .error_for_status()
        .map_err(|e| AcquireError::Other(anyhow::anyhow!("PDF download failed: {e}")))?
        .bytes()
        .await
        .map_err(|e| AcquireError::Other(anyhow::anyhow!("PDF download body failed: {e}")))?;
    tokio::fs::write(&pdf_path, &bytes).await?;

    let asin_str = object_asin_for(library, &primary_req).await;
    let mut primary_pdf_key = None;
    let mut written_keys = Vec::new();
    for dest in &destinations.items {
        let dest_req = request_for_destination(&primary_req, dest);
        let audio_key = planned_storage_key(library, &dest_req).await;
        let pdf_key = sidecar_key(&audio_key, "pdf");
        let meta = sidecar_meta(&asin_str, &dest_req.title, "application/pdf", &pdf_path).await;
        dest.backend.put_file(&pdf_key, &pdf_path, meta).await?;
        if dest.kind == destinations.primary {
            primary_pdf_key = Some(pdf_key.clone());
        }
        written_keys.push(pdf_key);
    }
    let pdf_key = primary_pdf_key
        .or_else(|| written_keys.first().cloned())
        .unwrap_or_default();
    library
        .set_pdf_status(
            &primary_req.asin,
            &primary_req.account_id,
            AcquireStatus::Acquired,
            Some(&pdf_key),
        )
        .await?;

    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    Ok(AcquireResult {
        asin: primary_req.asin.clone(),
        storage_key: pdf_key,
        written_keys,
        matched_existing: false,
    })
}
