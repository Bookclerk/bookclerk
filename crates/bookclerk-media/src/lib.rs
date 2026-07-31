//! Clear-media packaging: remux, metadata fix-up, and MP3 encode.
//!
//! DRM (Adrm / CENC) lives in store plugins (e.g. Audible), not here.

mod brand;
mod chapter_align;
mod chapters_mp4;
mod error;
mod metadata;
mod mp3;
mod mp4;
mod native;
mod package_m4b;

pub use brand::{
    brand_durations_from_chapter_info, brand_trim_range, rebase_chapters_after_brand_trim,
    runtime_length_ms_from_chapter_info, BrandDurations,
};
pub use chapter_align::{align_chapter_starts, scale_chapters_to_duration, ChapterAlignOptions};
pub use error::{MediaError, Result};
pub use metadata::{bookclerk_tool_tag, fixup_audiobook, FixupRequest, BOOKCLERK_TOOL_NAME};
pub use mp4::{
    extract_mp4a_config, parse_mp4, remux_progressive, track_duration_ms, Mp4aConfig, RemuxOptions,
    SampleEntryKind, TrimRange,
};
pub use native::remux_trimmed;
pub use package_m4b::{package_m4b_from_mp3, package_m4b_from_pcm, PackageM4bRequest};

use std::path::{Path, PathBuf};

/// Outcome of a successful media operation.
#[derive(Debug, Clone)]
pub struct MediaOutcome {
    pub output: PathBuf,
}

/// Re-encode audio to MP3 via Symphonia + LAME (classic Libation `DecryptToLossy`).
pub async fn encode_to_mp3(
    input: &Path,
    output: &Path,
    lame: &bookclerk_config::LameConfig,
    max_sample_rate: Option<u32>,
) -> Result<MediaOutcome> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    let lame = lame.clone();
    tokio::task::spawn_blocking(move || {
        mp3::encode_to_mp3_native(&input, &output, &lame, max_sample_rate)
    })
    .await
    .map_err(|err| MediaError::Native(format!("mp3 encode task join error: {err}")))?
}

/// Copy/trim a progressive M4B/M4A into a new file (chapter split helper).
pub async fn remux_trimmed_async(
    input: &Path,
    output: &Path,
    trim: TrimRange,
) -> Result<MediaOutcome> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    tokio::task::spawn_blocking(move || remux_trimmed(&input, &output, trim))
        .await
        .map_err(|err| MediaError::Native(format!("remux task join error: {err}")))?
}
