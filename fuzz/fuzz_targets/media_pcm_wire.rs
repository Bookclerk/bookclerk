//! Coverage-guided fuzzing of the PCM worker frames
//! (`bookclerk-media::pcm_wire`): requests the worker reads, and the
//! replies and reply streams the host reads. The oracle in
//! `fuzz/oracles/media_pcm_wire.rs` is shared with the crate's corpus
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/media_pcm_wire.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
