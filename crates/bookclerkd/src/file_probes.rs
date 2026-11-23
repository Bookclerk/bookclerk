//! The `file_probe` job: probe the audio files on one node's destinations
//! that were never probed.
//!
//! An acquire or an upload probes each audio file it stores and keeps the
//! result on the file's `edition_files` row (media type, bitrate,
//! losslessness, source store, running time). Rows written before probes
//! were recorded, and rows whose probe failed (no confined media worker
//! while `media.isolation = "required"`, or a file the tag reader could not
//! parse), have no `probed_at`; comparing two copies of an edition treats
//! such a row as unknown quality.
//!
//! Every heartbeat, at most once per [`PROBE_INTERVAL`], checks whether a
//! destination deployed on this node has such a row and, when one does,
//! queues a `file_probe` job pinned to this node. The job walks those rows
//! in `(destination, storage_key)` pages, reads each file's tags through
//! its destination's ranged reads with the confined tag reader
//! ([`bookclerk_media::MediaPool::read_tags`]), and stores the probe and
//! the stream. Each file is read and written under the daemon's work lock,
//! which every acquire holds from recording a key until its file is
//! written, and the probe is stored only while the row still carries the
//! `written_at` the walk read, so a file replaced around the walk keeps no
//! probe of the old bytes. A probed row drops out of the walk, so a job that stops
//! part way (cancelled, or its node restarted) resumes where it left off
//! when the next one runs. A file that still cannot be probed is skipped
//! and tried again by the next job.

use bookclerk_library::{
    AudioStream, EnqueueJobSpec, FileProbe, JobKind, JobPayload, JobTrigger, LibraryStore,
    UnprobedFile,
};
use chrono::Utc;
use tracing::{debug, warn};

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};
use crate::node_api::uploads::{owner_destinations, OwnerDestination};
use crate::samples::StoredFile;

/// How often a node looks for unprobed files: a daily walk retries files
/// that could not be probed (a media worker installed since, say) without
/// reading them on every beat.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Rows read per page of the walk.
const PROBE_PAGE: u64 = 64;

/// How long a row must stand unchanged before the walk reads its file: an
/// acquire records the key before it writes the bytes and probes them
/// itself once they are stored, so a row written moments ago may name a
/// file about to be replaced.
const PROBE_SETTLE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The `written_before` cutoff for rows that have stood unchanged for
/// `settle`.
fn settled_before(settle: std::time::Duration) -> String {
    chrono::Duration::from_std(settle)
        .map(|d| Utc::now() - d)
        .unwrap_or_else(|_| Utc::now())
        .to_rfc3339()
}

/// The payload of `node_id`'s probe walk.
fn probe_payload(node_id: &str) -> JobPayload {
    JobPayload {
        trigger: JobTrigger::Scheduler,
        node_id: Some(node_id.to_owned()),
        ..JobPayload::default()
    }
}

/// The ids of `destinations`.
fn destination_ids(destinations: &[OwnerDestination]) -> Vec<String> {
    destinations.iter().map(|d| d.id.clone()).collect()
}

/// Admits a `file_probe` job pinned to `node_id`.
///
/// # Returns
///
/// The admission outcome; a walk already queued or running for the node
/// is a [`AdmitJob::Duplicate`].
///
/// # Errors
///
/// Returns the admission error.
pub(crate) async fn enqueue_file_probe(
    state: &AppState,
    node_id: &str,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::FileProbe,
            payload: probe_payload(node_id),
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Queues a probe walk for `node_id` when none was admitted in the last
/// [`PROBE_INTERVAL`] and a destination deployed here has an audio file
/// that was never probed (and has stood for [`PROBE_SETTLE`]). The
/// heartbeat calls it once its beat is recorded.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `node_id` - This node.
pub(crate) async fn queue_file_probe_if_due(state: &AppState, node_id: &str) {
    queue_file_probe_settled(state, node_id, PROBE_SETTLE).await;
}

/// [`queue_file_probe_if_due`] for rows that have stood for `settle`.
async fn queue_file_probe_settled(state: &AppState, node_id: &str, settle: std::time::Duration) {
    let library = state.library_snapshot().await;
    let since = chrono::Duration::from_std(PROBE_INTERVAL)
        .map(|d| Utc::now() - d)
        .unwrap_or_else(|_| Utc::now());
    let key = JobKind::FileProbe.dedup_key(&probe_payload(node_id));
    match library.job_admitted_since(&key, since).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            warn!(error = %err, "could not read recent file probe walks");
            return;
        }
    }
    let Ok(destinations) = owner_destinations(state).await else {
        // No destination deployed here: nothing this node could read.
        return;
    };
    match library
        .unprobed_audio_files(
            &destination_ids(&destinations),
            &settled_before(settle),
            None,
            1,
        )
        .await
    {
        Ok(rows) if rows.is_empty() => return,
        Ok(_) => {}
        Err(err) => {
            warn!(error = %err, "could not look for unprobed audio files");
            return;
        }
    }
    match enqueue_file_probe(state, node_id).await {
        Ok(AdmitJob::Created(id)) => debug!(job_id = %id, "file probe walk queued"),
        Ok(AdmitJob::Duplicate(_)) => {}
        Ok(AdmitJob::QueueFull) => warn!("job queue is full; file probe walk not queued"),
        Err(err) => warn!(error = %err, "could not queue the file probe walk"),
    }
}

/// What the walk did with one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probed {
    /// The probe and stream are stored.
    Stored,
    /// The file could not be read or parsed; the next walk tries again.
    Unreadable,
    /// The key changed hands, left, or was written again since the page
    /// was read.
    Moved,
}

/// Probes one file from `destination` and stores what it found.
///
/// # Errors
///
/// Returns the library error of a failed write.
async fn probe_one(
    library: &LibraryStore,
    destination: &OwnerDestination,
    file: &UnprobedFile,
) -> bookclerk_library::Result<Probed> {
    let key = file.storage_key.as_str();
    let size = match destination.backend.head(key).await {
        Ok(Some(head)) => head.size,
        Ok(None) => return Ok(Probed::Unreadable),
        Err(err) => {
            debug!(destination = %destination.id, key, error = %err, "could not stat a file to probe");
            return Ok(Probed::Unreadable);
        }
    };
    let source = StoredFile {
        backend: destination.backend.clone_box(),
        key: key.to_owned(),
    };
    let tags = match bookclerk_media::pool().read_tags(size, &source).await {
        Ok(tags) => tags,
        Err(err) => {
            debug!(destination = %destination.id, key, error = %err, "could not probe a stored file");
            return Ok(Probed::Unreadable);
        }
    };
    let probed = bookclerk_media::AudioProbe::from_record(size, &tags);
    let probe = FileProbe {
        content_type: probed.content_type.to_string(),
        bitrate_kbps: probed
            .bitrate_kbps
            .map(|kbps| kbps.min(FileProbe::MAX_BITRATE_KBPS)),
        lossless: probed.lossless,
        source_store: library.sole_store_of_edition(file.owner.id()).await?,
    };
    let stream = AudioStream {
        codec: probed.codec,
        format: probed.format,
        duration_ms: probed.duration_ms,
    };
    if !library
        .set_backfilled_file_probe(file, &probe, &stream)
        .await?
    {
        return Ok(Probed::Moved);
    }
    Ok(Probed::Stored)
}

/// Runs a `file_probe` job on the node it is pinned to.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `ctx` - The claim, for progress and cancellation.
///
/// # Returns
///
/// A one-line summary for the job row.
///
/// # Errors
///
/// Fails when the job is cancelled or the library cannot be read or
/// written; the next walk picks up the rows left unprobed.
pub(crate) async fn run_file_probe(
    state: &AppState,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    walk_file_probes(state, ctx, PROBE_SETTLE).await
}

/// [`run_file_probe`] over rows that have stood for `settle`.
async fn walk_file_probes(
    state: &AppState,
    ctx: Option<&JobExecCtx>,
    settle: std::time::Duration,
) -> anyhow::Result<String> {
    let written_before = settled_before(settle);
    let library = state.library_snapshot().await;
    let destinations = owner_destinations(state).await?;
    let ids = destination_ids(&destinations);
    let mut after: Option<(String, String)> = None;
    let (mut stored, mut unreadable, mut moved) = (0u64, 0u64, 0u64);
    loop {
        if job_cancelled(ctx, &library).await {
            anyhow::bail!("cancelled");
        }
        let page = library
            .unprobed_audio_files(
                &ids,
                &written_before,
                after.as_ref().map(|(d, k)| (d.as_str(), k.as_str())),
                PROBE_PAGE,
            )
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        after = Some((last.destination.clone(), last.storage_key.clone()));
        for file in &page {
            let Some(destination) = destinations.iter().find(|d| d.id == file.destination) else {
                continue;
            };
            // Acquires record a row before they write its file and hold
            // the work lock until the write is done: holding it here keeps
            // the probe from reading a file mid-replacement.
            let _writes = state.work_lock.lock().await;
            // The lock may have been held for a long acquire: a cancel
            // that came meanwhile stops the walk before the next read.
            if ctx.is_some_and(JobExecCtx::is_cancelled) {
                anyhow::bail!("cancelled");
            }
            match probe_one(&library, destination, file).await? {
                Probed::Stored => stored += 1,
                Probed::Unreadable => unreadable += 1,
                Probed::Moved => moved += 1,
            }
        }
        if let Some(ctx) = ctx {
            let _ = library
                .set_job_progress(
                    &ctx.fence,
                    &format!("{stored} file(s) probed, {unreadable} unreadable"),
                )
                .await;
        }
    }
    Ok(format!(
        "{stored} file(s) probed, {unreadable} could not be read, {moved} changed hands"
    ))
}

#[cfg(test)]
#[path = "file_probes_tests.rs"]
mod tests;
