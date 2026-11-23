//! The `chapter_align` job: align a metadata provider's chapter layout to
//! one edition's stored copy (`docs/adr/edition-chapters.md`, decision 3).
//!
//! An upload with an ASIN queues one once its files are stored, and an
//! Administrator queues one from the edition's chapters
//! (`POST /api/library/editions/{id}/chapters/realign`). The job asks the
//! providers for the edition's layout by its ASIN, reads the copy's audio
//! from the first of this node's destinations that holds every file with a
//! known running time, aligns the layout to it
//! ([`bookclerk_acquire::realign_copy`]) and records the provider and
//! processed claims; the resolver decides what the edition plays.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use bookclerk_acquire::{realign_copy, CopyAudio, RealignOutcome, RealignTarget};
use bookclerk_config::OutputCopy;
use bookclerk_library::{
    EnqueueJobSpec, FileRole, JobKind, JobPayload, JobTrigger, LibraryStore, ProbedAudioFile,
};
use tokio::io::AsyncWriteExt;

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};
use crate::node_api::uploads::{owner_destinations, OwnerDestination};

/// Folder under the download cache that holds each alignment's copy while
/// it runs; the scratch sweep clears what a crash leaves.
pub(crate) const SCRATCH_DIR: &str = "chapter-align";

/// Queues the alignment of `edition_id`'s chapters.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `edition_id` - The edition (`editions.id`).
/// * `trigger` - Who asked: an Administrator (`api`) or an upload
///   (`scheduler`).
///
/// # Returns
///
/// The admission outcome; an alignment of the edition already queued or
/// running is a [`AdmitJob::Duplicate`].
///
/// # Errors
///
/// Returns the admission error.
pub(crate) async fn enqueue_chapter_align(
    state: &AppState,
    edition_id: &str,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::ChapterAlign,
            payload: JobPayload {
                title: Some(edition_id.to_string()),
                trigger,
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Runs a `chapter_align` job.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `payload` - The job's payload; [`JobPayload::title`] is the edition.
/// * `ctx` - The claim, for cancellation and the scratch registration.
///
/// # Returns
///
/// A one-line summary for the job row. An edition with no ASIN, no copy
/// on this node or no provider layout finishes with nothing recorded.
///
/// # Errors
///
/// Fails when the job is cancelled, the library cannot be read, or the
/// copy cannot be read from its destination.
pub(crate) async fn run_chapter_align(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let edition_id = payload
        .title
        .as_deref()
        .filter(|id| !id.is_empty())
        .context("chapter_align names no edition")?;
    let library = state.library_snapshot().await;
    let asins = library.edition_asins(edition_id).await?;
    let Some((asin, marketplace)) = asins.first() else {
        return Ok(format!(
            "edition {edition_id} has no ASIN to ask providers by"
        ));
    };
    let destinations = owner_destinations(state).await?;
    let files = library.edition_audio_files(edition_id).await?;
    let Some((destination, copy)) = copy_here(&destinations, &files) else {
        return Ok(format!(
            "edition {edition_id} has no copy with known running times on this node"
        ));
    };
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let scratch = state
        .config
        .read()
        .await
        .download_cache_dir()
        .join(SCRATCH_DIR)
        .join(uuid::Uuid::new_v4().simple().to_string());
    tokio::fs::create_dir_all(&scratch).await?;
    let scratch_path = scratch.to_string_lossy().into_owned();
    if let Some(ctx) = ctx {
        library
            .register_job_temp_path(&ctx.fence.job_id, &scratch_path)
            .await?;
    }
    let outcome = align_in(
        state,
        &library,
        edition_id,
        (asin, marketplace),
        destination,
        &copy,
        &scratch,
        ctx,
    )
    .await;
    if let Err(err) = tokio::fs::remove_dir_all(&scratch).await {
        tracing::warn!(path = %scratch.display(), error = %err, "could not remove alignment scratch");
    }
    if let Some(ctx) = ctx {
        if let Err(err) = library
            .unregister_job_temp_path(&ctx.fence.job_id, &scratch_path)
            .await
        {
            tracing::warn!(error = %err, "could not unregister alignment scratch");
        }
    }
    Ok(match outcome? {
        RealignOutcome::NoAudio => format!("edition {edition_id}: the copy has no running time"),
        RealignOutcome::NoLayout => {
            format!("edition {edition_id}: no provider has a chapter layout")
        }
        RealignOutcome::Recorded { provider, chapters } => format!(
            "edition {edition_id}: {chapters} chapters from {provider} aligned to the copy on {}",
            destination.id
        ),
    })
}

/// Reads `copy` from `destination` into `scratch` and aligns the provider
/// layout for `asin` to it.
///
/// # Errors
///
/// Fails when the job is cancelled or a file cannot be read.
#[allow(clippy::too_many_arguments)] // One call site; a struct would only rename them.
async fn align_in(
    state: &AppState,
    library: &LibraryStore,
    edition_id: &str,
    (asin, marketplace): (&str, &str),
    destination: &OwnerDestination,
    copy: &[&ProbedAudioFile],
    scratch: &Path,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<RealignOutcome> {
    let mut audio = Vec::with_capacity(copy.len());
    for (index, file) in copy.iter().enumerate() {
        if job_cancelled(ctx, library).await {
            anyhow::bail!("cancelled");
        }
        let path = scratch.join(format!("{index:04}.{}", extension(&file.storage_key)));
        read_to(destination, &file.storage_key, &path).await?;
        audio.push(CopyAudio {
            path,
            duration_ms: file.stream.duration_ms.unwrap_or_default(),
        });
    }
    let providers: Arc<dyn bookclerk_enrich::MetadataProviders> =
        Arc::new(state.integrations.read().await.clone());
    Ok(realign_copy(
        library,
        providers.as_ref(),
        RealignTarget {
            edition_id,
            asin,
            marketplace,
        },
        &audio,
        &destination.options,
    )
    .await)
}

/// Streams `key` on `destination` into the file at `path`.
///
/// # Errors
///
/// Fails when the object cannot be read or the file written.
async fn read_to(destination: &OwnerDestination, key: &str, path: &Path) -> anyhow::Result<()> {
    let (_, mut body) = destination
        .backend
        .get_stream(key, None)
        .await
        .with_context(|| format!("read {key} from {}", destination.id))?;
    let mut out = tokio::fs::File::create(path).await?;
    tokio::io::copy(&mut body, &mut out).await?;
    out.flush().await?;
    Ok(())
}

/// The first whole copy of the edition on `destinations` whose files all
/// have a probed running time, and that copy's files in play order.
///
/// Destinations are tried in this node's order. On each, the files of one
/// library copy (`output.copies`) are taken together, never mixed with
/// another copy's: an upload or unnamed copy first, then the named copies
/// in format and quality order. A copy's files are its primary audio, then
/// its parts in key order; its chapter files only when it has nothing
/// else, since they repeat the primary audio otherwise.
fn copy_here<'a>(
    destinations: &'a [OwnerDestination],
    files: &'a [ProbedAudioFile],
) -> Option<(&'a OwnerDestination, Vec<&'a ProbedAudioFile>)> {
    destinations.iter().find_map(|destination| {
        let here: Vec<&ProbedAudioFile> = files
            .iter()
            .filter(|file| file.destination == destination.id)
            .collect();
        let copies: BTreeSet<&Option<OutputCopy>> = here.iter().map(|file| &file.copy).collect();
        copies.into_iter().find_map(|which| {
            let on = |roles: &[FileRole]| -> Vec<&ProbedAudioFile> {
                let mut picked: Vec<&ProbedAudioFile> = here
                    .iter()
                    .copied()
                    .filter(|file| &file.copy == which && roles.contains(&file.role))
                    .collect();
                picked.sort_by_key(|file| (file.role != FileRole::Audio, file.storage_key.clone()));
                picked
            };
            let mut copy = on(&[FileRole::Audio, FileRole::Part]);
            if copy.is_empty() {
                copy = on(&[FileRole::Chapter]);
            }
            let timed = !copy.is_empty()
                && copy
                    .iter()
                    .all(|file| file.stream.duration_ms.is_some_and(|ms| ms > 0));
            timed.then_some((destination, copy))
        })
    })
}

/// The lower-case extension of `key`, which the decoder reads the format
/// hint from, or `bin` when it has none or an unusual one.
fn extension(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "bin".to_string())
}

#[cfg(test)]
#[path = "chapter_align_tests.rs"]
mod tests;
