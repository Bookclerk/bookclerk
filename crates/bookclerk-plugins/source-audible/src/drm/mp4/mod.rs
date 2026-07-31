//! Audible DRM remux (Adrm / CENC) — not used by the host packaging crate.

mod boxutil;
mod cenc;
mod dash;
mod parser;
mod remux;
mod samples;

pub use dash::{decrypt_dash_cenc, looks_like_dash};
pub use parser::{parse_mp4, SampleEntryKind};
pub use remux::{decrypt_and_remux, DecryptMode, RemuxOptions, TrimRange};

pub(crate) use cenc::{
    find_stbl_in_trak, parse_tenc_from_enca_entry, progressive_sample_ivs,
    sample_entry_end_from_type_offset,
};
