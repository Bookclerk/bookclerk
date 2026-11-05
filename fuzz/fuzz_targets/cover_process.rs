//! Coverage-guided fuzzing of the cover pipeline (`bookclerk-cover::process`):
//! sniff, decode under limits, resize and re-encode an untrusted image. The
//! oracle in `fuzz/oracles/cover_process.rs` is shared with the crate's corpus
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cover_process.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
