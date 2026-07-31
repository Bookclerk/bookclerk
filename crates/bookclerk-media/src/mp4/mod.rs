//! ISO-BMFF helpers for clear M4B remux / packaging (no DRM).

mod boxutil;
pub(crate) mod mux_aac;
mod parser;
mod remux;
mod samples;

pub use parser::{extract_mp4a_config, parse_mp4, track_duration_ms, Mp4aConfig, SampleEntryKind};
pub use remux::{remux_progressive, RemuxOptions, TrimRange};
