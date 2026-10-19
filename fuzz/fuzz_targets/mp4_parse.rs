//! Coverage-guided ISO-BMFF parsing and remux of untrusted store media.
//!
//! Store downloads reach `bookclerk-mp4` byte-for-byte, so every input is
//! attacker-shaped. The parser and remuxer take filesystem paths, so each input
//! is written to a per-process scratch file first; the in-memory box helpers
//! run on the raw bytes directly.

#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use bookclerk_mp4::{
    extract_mp4a_config, parse_mp4, remux_progressive, shift_chunk_offsets, strip_trailing_free,
    track_duration_ms, CopySamples, RemuxOptions, TrimRange,
};
use libfuzzer_sys::fuzz_target;

/// Largest input worth parsing; real headers fit well inside this.
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Scratch input and output paths, unique to this fuzzing process.
fn scratch_paths() -> &'static (PathBuf, PathBuf) {
    static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    PATHS.get_or_init(|| {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        (
            dir.join(format!("bookclerk-mp4-fuzz-{pid}.m4b")),
            dir.join(format!("bookclerk-mp4-fuzz-{pid}-out.m4b")),
        )
    })
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    // In-memory helpers used when patching an existing `moov`.
    let _ = bookclerk_mp4::edit::find_box_range(data, b"moov");
    let mut moov = data.to_vec();
    let _ = strip_trailing_free(&mut moov);
    let mut shifted = data.to_vec();
    let _ = shift_chunk_offsets(&mut shifted, 0, 8);

    let (input, output) = scratch_paths();
    if std::fs::write(input, data).is_err() {
        return;
    }
    let Ok(mp4) = parse_mp4(input) else {
        return;
    };
    let _ = track_duration_ms(&mp4.audio);
    let _ = extract_mp4a_config(&mp4);

    let trim = RemuxOptions {
        trim: Some(TrimRange {
            start_ms: 10,
            end_ms: None,
        }),
    };
    let _ = remux_progressive(input, output, &RemuxOptions::default(), &mut CopySamples);
    let _ = remux_progressive(input, output, &trim, &mut CopySamples);
});
