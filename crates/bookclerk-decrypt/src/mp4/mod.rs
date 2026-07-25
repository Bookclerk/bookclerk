//! Minimal ISO-BMFF helpers for Audible AAXC decrypt / remux.

mod boxutil;
pub(crate) mod cenc;
mod dash;
pub(crate) mod mux_aac;
mod parser;
mod remux;
mod samples;

pub use dash::{decrypt_dash_cenc, looks_like_dash};
pub use parser::{extract_mp4a_config, parse_mp4, track_duration_ms, Mp4aConfig, SampleEntryKind};
pub use remux::{decrypt_and_remux, DecryptMode, RemuxOptions, TrimRange};
