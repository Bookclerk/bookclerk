//! Add a copy to a book the library already stores (an administrator's
//! convert): the copy is made from the stored files, never from a new store
//! download, and goes through the same copy path an acquire uses.

use std::path::{Path, PathBuf};

use bookclerk_config::{CopyFormat, OutputCopy};
use bookclerk_library::{AcquireStatus, FileOwner, FileRole, LibraryStore, ProbedAudioFile};
use bookclerk_source::{PlainAudioPart, PlainFetch};
use bookclerk_storage::StorageBackend;
use tokio::io::AsyncWriteExt;

use super::{
    cleanup_work_dir, placed_destinations, prepare_work_dir, record_copies, remaining_for_new_file,
    resolve_title, status_key, stored_copies, store_copies, AcquireRequest, AcquireResult,
    CopyOutcome, CopyResult, CopyTarget,
};
use crate::destinations::AcquireDestinations;
use crate::error::{AcquireError, Result};
use crate::library_copies::LibraryCopySource;
use crate::record::recording_copy_destinations;

/// Scratch folder under an acquire's work directory that holds the stored
/// files a convert reads.
const SOURCE_DIR: &str = "convert-source";

/// Adds `copy` to the edition of the title `req` names, made from a copy
/// the library already stores, on every destination whose filter takes
/// that copy.
///
/// The source is the best stored copy the destinations hold: one kept as
/// delivered or as an M4B before an encoded one, then the main output's
/// copy, then the primary destination's. Its files are downloaded into
/// scratch and packaged like a store download, so tags, chapters, cover
/// and naming follow the copy's options. The new files are recorded as
/// `copy`'s on the edition (`edition_files`); the title's storage key and
/// its main copy are left as they are. No store is asked.
///
/// # Arguments
///
/// * `library` - Library store holding the edition's recorded files.
/// * `destinations` - Every reachable destination, with its current filter.
/// * `req` - The title, as an acquire of it would name it; `force` makes
///   the copy again when the edition already has it.
/// * `copy` - The copy to add.
///
/// # Returns
///
/// What was written: `matched_existing` is true when the edition already
/// had the copy and `force` was off.
///
/// # Errors
///
/// Returns [`AcquireError::Other`] for a copy that is not a new encoding
/// (`original`), the library's main output, a title that has no library
/// row or is not stored, or an edition with no stored audio on a reachable
/// destination; [`AcquireError::NoDestinationAccepts`] when no destination
/// takes the book or the copy; and the download, packaging or storage error
/// otherwise.
pub async fn convert_copy(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    mut req: AcquireRequest,
    copy: &OutputCopy,
) -> Result<AcquireResult> {
    copy.validate()
        .map_err(|err| AcquireError::Other(anyhow::anyhow!("{err}")))?;
    if copy.format == CopyFormat::Original {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "a convert makes a new encoding; an `original` copy keeps the store's files and \
             comes only from a download"
        )));
    }
    if destinations.is_empty() {
        return Err(AcquireError::NoDestinationAccepts);
    }
    let primary = destinations.primary_destination();
    let main = OutputCopy::main(primary.options.format, &primary.options.lame);
    if main.as_ref() == Some(copy) {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "{copy} is the library's main output already"
        )));
    }
    let title = resolve_title(library, &req).await.ok_or_else(|| {
        AcquireError::Other(anyhow::anyhow!(
            "{}: the title has no row in the library",
            req.asin
        ))
    })?;
    let edition = title.edition_id.clone();
    let stored = library
        .edition_acquisition(&edition)
        .await?
        .is_some_and(|acquisition| acquisition.status == AcquireStatus::Acquired);
    if !stored {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "{}: the book is not stored yet; acquire it before converting it",
            req.asin
        )));
    }
    let owner = FileOwner::Edition(edition.clone());
    if req.call_number.is_none() {
        req.call_number = bookclerk_library::call_number(&edition);
    }

    let placed = placed_destinations(destinations, &req)?;
    let target_set = placed.as_ref().unwrap_or(destinations);
    let files = library.edition_audio_files(&edition).await?;

    let existing: Vec<&ProbedAudioFile> = files
        .iter()
        .filter(|file| file.copy.as_ref() == Some(copy))
        .filter(|file| target_set.items.iter().any(|dest| dest.id.as_str() == file.destination))
        .collect();
    if !req.force {
        if let Some(found) = existing.iter().find(|file| file.role == FileRole::Audio) {
            tracing::info!(
                id = %status_key(&req),
                copy = %copy,
                key = %found.storage_key,
                "the edition already has this copy"
            );
            return Ok(AcquireResult {
                asin: req.asin,
                storage_key: found.storage_key.clone(),
                written_keys: Vec::new(),
                matched_existing: true,
                from_library: false,
                store_fetches: 0,
                copies: Vec::new(),
            });
        }
    }

    let (source_dest, source_files) = source_copy(&files, destinations, main.as_ref(), copy)
        .ok_or_else(|| {
            AcquireError::Other(anyhow::anyhow!(
                "{}: no reachable destination holds a stored copy to convert from",
                req.asin
            ))
        })?;

    let mut target = target_set
        .added_copy_plan(copy)
        .ok_or(AcquireError::NoDestinationAccepts)?;
    for item in &mut target.items {
        // The main output keeps the title's companion PDF; a convert adds
        // audio only.
        item.options.download_pdf = false;
    }
    // A convert never reads a library this install joined: the copy is
    // made from this library's own files, and its probe names the store
    // the source copy came from.
    req.library_copies = LibraryCopySource::default().for_one_acquire();
    if let Some(store) = source_files
        .iter()
        .find_map(|file| file.probe.as_ref()?.source_store.as_deref())
    {
        req.library_copies.stored_from().set(store);
    }
    let recorded = recording_copy_destinations(
        library,
        &target,
        Some(&owner),
        &req.source,
        req.library_copies.stored_from(),
        copy,
    );
    req.options = recorded.primary_destination().options.clone();
    req.destination_keys =
        crate::keys::DestinationKeys::of(recorded.primary_destination().backend.as_ref());

    tracing::info!(
        id = %status_key(&req),
        copy = %copy,
        from = %source_dest.id,
        files = source_files.len(),
        destinations = recorded.len(),
        "converting a stored book into another copy"
    );
    let work_dir = req.cache_dir.join("acquire").join(status_key(&req));
    let result = async {
        prepare_work_dir(library, &req, &work_dir).await?;
        let fetch = download_source(
            library,
            &req,
            &work_dir,
            source_dest.backend.as_ref(),
            &source_files,
        )
        .await?;
        let targets = [CopyTarget {
            copy: copy.clone(),
            destinations: recorded,
        }];
        store_copies(library, &targets, &req, &work_dir, fetch, None).await
    }
    .await;
    cleanup_work_dir(library, req.job_id.as_deref(), &work_dir).await;
    let mut result = result?;
    result.matched_existing = false;
    result.from_library = false;
    result.store_fetches = 0;

    // A destination that held no copy of the edition before now holds
    // this one; one that holds the main output keeps its key.
    let held = stored_copies(library, &edition, target_set).await?;
    let new_holders: Vec<CopyOutcome> = result
        .copies
        .iter()
        .filter(|outcome| match &outcome.result {
            CopyResult::Stored { .. } => held.key_on(&outcome.destination).is_none(),
            CopyResult::Failed { .. } => false,
        })
        .cloned()
        .collect();
    record_copies(library, &edition, &new_holders).await?;
    Ok(result)
}

/// The stored copy a convert to `wanted` reads, and the destination it is
/// read from: among the copies on `destinations` other than `wanted`, one
/// kept as delivered or as an M4B before an encoded one (it has lost the
/// least), then the main output's copy (`main`), then the primary
/// destination's. Its files are in playing order: the primary audio file,
/// then parts and chapter files by key.
///
/// # Returns
///
/// `None` when no destination given holds an audio file of the edition.
fn source_copy<'a>(
    files: &'a [ProbedAudioFile],
    destinations: &'a AcquireDestinations,
    main: Option<&OutputCopy>,
    wanted: &OutputCopy,
) -> Option<(&'a crate::destinations::AcquireDestination, Vec<&'a ProbedAudioFile>)> {
    let mut groups: Vec<(&crate::destinations::AcquireDestination, Option<&OutputCopy>)> =
        Vec::new();
    for file in files {
        if file.copy.as_ref() == Some(wanted) {
            continue;
        }
        let Some(dest) = destinations
            .items
            .iter()
            .find(|dest| dest.id.as_str() == file.destination)
        else {
            continue;
        };
        let key = (dest, file.copy.as_ref());
        if !groups
            .iter()
            .any(|(d, c)| d.id == key.0.id && *c == key.1)
        {
            groups.push(key);
        }
    }
    let rank = |(dest, copy): &(&crate::destinations::AcquireDestination, Option<&OutputCopy>)| {
        let encoded = copy.is_some_and(|copy| copy.format.is_encoded());
        let not_main = copy != &main;
        let not_primary = dest.id != destinations.primary;
        (encoded, not_main, not_primary)
    };
    let (dest, copy) = groups.into_iter().min_by_key(rank)?;
    let mut chosen: Vec<&ProbedAudioFile> = files
        .iter()
        .filter(|file| file.destination == dest.id.as_str() && file.copy.as_ref() == copy)
        .collect();
    // The primary audio file first; parts and chapter files keep key order.
    chosen.sort_by_key(|file| (file.role != FileRole::Audio, file.storage_key.clone()));
    if chosen.iter().any(|file| file.role == FileRole::Audio) {
        chosen.truncate(1);
    }
    Some((dest, chosen))
}

/// Downloads the stored files of the source copy into scratch and
/// describes them as a store fetch would: one MP4 file as the fetch's
/// M4B, anything else as ordered parts with the running time the tag
/// reader finds. The chapters of a single file come from its own tags.
///
/// # Errors
///
/// Returns an error when the scratch budget is too small, a download fails
/// or comes out another size than the destination reported, or the tag
/// reader cannot read a file.
async fn download_source(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
    backend: &dyn StorageBackend,
    files: &[&ProbedAudioFile],
) -> Result<PlainFetch> {
    let dir = work_dir.join(SOURCE_DIR);
    tokio::fs::create_dir_all(&dir).await?;
    let media = bookclerk_media::pool();
    let mut fetch = PlainFetch {
        parts: Vec::new(),
        m4b_path: None,
        cover_path: None,
        chapters: Vec::new(),
        chapter_kind: bookclerk_source::ChapterKind::Unknown,
        pdf_url: None,
    };
    let single = files.len() == 1;
    for (index, file) in files.iter().enumerate() {
        let ext = extension_of(&file.storage_key);
        let path = dir.join(format!("{index:04}.{ext}"));
        let budget = remaining_for_new_file(library, req, work_dir).await?;
        let size = download_capped(backend, &file.storage_key, &path, budget).await?;
        let tags = media
            .read_tags(size, &bookclerk_media::FileRange::new(&path))
            .await?;
        if single {
            fetch.chapters = tags
                .chapters
                .iter()
                .map(|chapter| (chapter.title.clone(), chapter.start_ms))
                .collect();
            if !fetch.chapters.is_empty() {
                fetch.chapter_kind = bookclerk_source::ChapterKind::Chapters;
            }
            if tags.container == bookclerk_tags::Container::Mp4 {
                fetch.m4b_path = Some(path);
                continue;
            }
        }
        fetch.parts.push(PlainAudioPart {
            path,
            title: None,
            duration_ms: tags.duration.map(|d| d.ms).filter(|ms| *ms > 0),
        });
    }
    Ok(fetch)
}

/// The lowercase extension of a storage key's file name, or `bin`; only
/// ASCII letters and digits are kept, so a key never names a scratch path.
fn extension_of(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    let ext: String = name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if ext.is_empty() {
        "bin".to_owned()
    } else {
        ext
    }
}

/// Streams `key` from `backend` into `path`, refusing more than `budget`
/// bytes and a body of another size than the destination reported.
///
/// # Returns
///
/// The bytes written.
///
/// # Errors
///
/// Returns an error when the read or write fails, the object is larger than
/// `budget`, or its size does not match.
async fn download_capped(
    backend: &dyn StorageBackend,
    key: &str,
    path: &PathBuf,
    budget: u64,
) -> Result<u64> {
    let (probe, mut body) = backend.get_stream(key, None).await?;
    if probe.size > budget {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "the stored file is {} bytes and {budget} are left for scratch",
            probe.size
        )));
    }
    let mut file = tokio::fs::File::create(path).await?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut body, &mut buf).await?;
        if n == 0 {
            break;
        }
        let more = u64::try_from(n).unwrap_or(u64::MAX);
        written = written.saturating_add(more);
        if written > budget || (probe.size > 0 && written > probe.size) {
            return Err(AcquireError::Storage(
                bookclerk_storage::StorageError::Integrity(format!(
                    "read {written} bytes of `{key}`, which is {} bytes",
                    probe.size
                )),
            ));
        }
        file.write_all(&buf[..n]).await?;
    }
    file.sync_all().await?;
    if probe.size > 0 && written != probe.size {
        return Err(AcquireError::Storage(
            bookclerk_storage::StorageError::Integrity(format!(
                "read {written} bytes of `{key}`, which is {} bytes",
                probe.size
            )),
        ));
    }
    if written == 0 {
        return Err(AcquireError::Other(anyhow::anyhow!("`{key}` is empty")));
    }
    Ok(written)
}

#[cfg(test)]
#[path = "convert_copy_tests.rs"]
mod tests;
