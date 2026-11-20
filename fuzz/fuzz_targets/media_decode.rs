//! Coverage-guided fuzzing of the media worker's demux and decode path
//! (`bookclerk-media::decode_pcm_bytes`) over whole untrusted audio files.
//! The oracle in `fuzz/oracles/media_decode.rs` is shared with the crate's
//! corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/media_decode.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
