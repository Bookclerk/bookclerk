//! Audible-specific MP4 reading: Common Encryption metadata and DASH assembly.
//!
//! The container plumbing these sit on is [`bookclerk_mp4`]; what is left here
//! is the part that only means something to a protected download.

mod cenc;
mod dash;

pub use dash::{decrypt_dash_cenc, looks_like_dash};

pub(crate) use cenc::{
    find_stbl_in_trak, parse_tenc_from_enca_entry, progressive_sample_ivs,
    sample_entry_end_from_type_offset,
};
