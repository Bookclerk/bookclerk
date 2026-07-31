//! Clear-media remux helpers (no DRM).

use std::path::Path;

use crate::error::{MediaError, Result};
use crate::mp4::{remux_progressive, RemuxOptions, TrimRange};
use crate::MediaOutcome;

/// Remux a progressive clear M4B/M4A with an optional media-time trim (chapter split).
pub fn remux_trimmed(input: &Path, output: &Path, trim: TrimRange) -> Result<MediaOutcome> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    remux_progressive(
        input,
        output,
        &RemuxOptions {
            trim: Some(trim),
            rewrite_ftyp: true,
        },
    )?;
    if !output.exists() {
        return Err(MediaError::OutputMissing(output.to_path_buf()));
    }
    Ok(MediaOutcome {
        output: output.to_path_buf(),
    })
}
