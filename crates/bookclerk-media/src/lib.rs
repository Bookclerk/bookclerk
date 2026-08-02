//! Clear-media packaging: remux, metadata fix-up, and MP3 encode.
//!
//! DRM (Adrm / CENC) lives in store plugins (e.g. Audible), not here. The
//! ISO-BMFF reading and writing underneath is [`bookclerk_mp4`], which both this
//! crate and those plugins share; the keys and ciphers stay with the plugin.
//!
//! # Where the work runs
//!
//! The async entry points in this module do not decode anything themselves.
//! They build a [`MediaJob`] and hand it to a [`MediaPool`], which runs it in a
//! short-lived child process confined to the paths that job declared. Hosts
//! install the pool once at startup with [`init_pool`]; anything that skips
//! that gets a default pool, which still isolates as long as the worker binary
//! is installed alongside the host.
//!
//! The synchronous functions ([`remux_trimmed`], [`align_chapter_starts`],
//! [`package_m4b_from_pcm`], and the MP4 parsing helpers) run in-process by
//! design. They are cheap, take already-parsed input, or are the implementation
//! the worker itself calls.

mod brand;
mod chapter_align;
mod chapters_mp4;
mod error;
mod job;
mod metadata;
mod moov;
mod mp3;
mod mux_aac;
mod native;
mod package_m4b;
mod pool;

/// Container plumbing lives in [`bookclerk_mp4`]; these are re-exported so
/// callers of this crate do not need a second dependency to read an M4B.
pub use bookclerk_mp4::{
    extract_mp4a_config, parse_mp4, track_duration_ms, Mp4aConfig, SampleEntryKind, TrimRange,
};
pub use brand::{
    brand_durations_from_chapter_info, brand_trim_range, rebase_chapters_after_brand_trim,
    runtime_length_ms_from_chapter_info, BrandDurations,
};
pub use chapter_align::{align_chapter_starts, scale_chapters_to_duration, ChapterAlignOptions};
pub use error::{MediaError, Result};
pub use job::{MediaJob, MediaJobOutput, MediaJobReply};
pub use metadata::{bookclerk_tool_tag, fixup_audiobook, FixupRequest, BOOKCLERK_TOOL_NAME};
pub use native::remux_trimmed;
pub use package_m4b::{package_m4b_from_mp3, package_m4b_from_pcm, PackageM4bRequest};
pub use pool::{
    init_pool, init_pool_from_config, pool, replace_pool, Confinement, MediaPool, MediaPoolConfig,
    JAIL_BIN_ENV, JAIL_BIN_NAME, WORKER_BIN_ENV, WORKER_BIN_NAME, WORKER_ENFORCEMENT_ENV,
};

use std::path::{Path, PathBuf};

/// Outcome of a successful media operation.
#[derive(Debug, Clone)]
pub struct MediaOutcome {
    pub output: PathBuf,
}

/// Re-encode audio to MP3 via Symphonia + LAME (classic Libation `DecryptToLossy`).
///
/// Runs in a confined media worker; see the module documentation.
///
/// # Errors
///
/// Returns [`MediaError::InputMissing`] when `input` does not exist, and
/// propagates encode and worker failures otherwise.
pub async fn encode_to_mp3(
    input: &Path,
    output: &Path,
    lame: &bookclerk_config::LameConfig,
    max_sample_rate: Option<u32>,
) -> Result<MediaOutcome> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
    }
    let output = output.to_path_buf();
    // Bound rather than used inline: holding the handle for the whole job is
    // what lets a config reload swap the pool without disturbing this one.
    let pool = pool();
    pool.run(MediaJob::EncodeMp3 {
        input: input.to_path_buf(),
        output: output.clone(),
        lame: Box::new(lame.clone()),
        max_sample_rate,
    })
    .await?;
    if !output.exists() {
        return Err(MediaError::OutputMissing(output));
    }
    Ok(MediaOutcome { output })
}

/// Copy/trim a progressive M4B/M4A into a new file (chapter split helper).
///
/// Runs in a confined media worker; see the module documentation.
///
/// # Errors
///
/// Returns [`MediaError::InputMissing`] when `input` does not exist, and
/// propagates remux and worker failures otherwise.
pub async fn remux_trimmed_async(
    input: &Path,
    output: &Path,
    trim: TrimRange,
) -> Result<MediaOutcome> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
    }
    let output = output.to_path_buf();
    let pool = pool();
    pool.run(MediaJob::RemuxTrimmed {
        input: input.to_path_buf(),
        output: output.clone(),
        trim,
    })
    .await?;
    if !output.exists() {
        return Err(MediaError::OutputMissing(output));
    }
    Ok(MediaOutcome { output })
}

/// Snap chapter starts to spoken-title onsets by local waveform analysis.
///
/// The synchronous [`align_chapter_starts`] decodes audio on the calling
/// thread, which on an async runtime blocks a worker for the length of the
/// analysis. Prefer this from async code.
///
/// Analysis never fails the acquire: a decode problem leaves the original
/// chapter starts in place, matching the synchronous behaviour.
pub async fn align_chapter_starts_async(
    path: &Path,
    chapters: &[(String, u64)],
    options: ChapterAlignOptions,
) -> Vec<(String, u64)> {
    let job = MediaJob::AlignChapters {
        path: path.to_path_buf(),
        chapters: chapters.to_vec(),
        options,
    };
    let pool = pool();
    match pool.run(job).await {
        Ok(output) => output
            .chapters()
            .map(<[(String, u64)]>::to_vec)
            .unwrap_or_else(|| chapters.to_vec()),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "chapter align failed; keeping original chapter starts"
            );
            chapters.to_vec()
        }
    }
}
