//! ISO-BMFF (MP4) container plumbing: read a progressive file, pick samples by
//! media time, and write a faststart `ftyp` + `moov` + `mdat` M4B in one pass.
//!
//! # No cryptography here
//!
//! [`remux_progressive`] streams payloads through a [`SampleTransform`], and the
//! only implementation this crate ships is [`CopySamples`]. A store that has to
//! turn its own ciphertext into plaintext keeps both the key and the cipher on
//! its side of that trait, inside its own plugin process.
//!
//! That split is why two very similar remuxers used to exist: the host needed
//! one for clear media and the Audible plugin needed one that could decrypt
//! mid-copy, and neither could take on the other's dependencies. The transform
//! hook removes the copy without moving the boundary — see `docs/media.md` for
//! why the boundary is where it is.

pub mod boxutil;
pub mod edit;
mod error;
#[cfg(feature = "fixtures")]
pub mod fixture;
mod parser;
mod read;
mod remux;
mod samples;

pub use error::{Mp4Error, Result};
pub use parser::{
    extract_mp4a_config, parse_mp4, track_duration_ms, AudioTrack, Mp4File, Mp4aConfig,
    SampleEntryKind,
};
pub use read::SampleReader;
pub use remux::{
    remux_progressive, write_progressive_m4b, CopySamples, ProgressiveWriteInput, RemuxOptions,
    SampleTransform, TrimRange,
};
pub use samples::{build_samples, select_samples_by_ms, ChunkMapEntry, SampleInfo};
