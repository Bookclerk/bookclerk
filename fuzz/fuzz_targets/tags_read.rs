//! Coverage-guided fuzzing of the embedded tag reader (`bookclerk-tags::TagReader`)
//! over whole untrusted media files: MP4, MP3 and FLAC. The oracle in
//! `fuzz/oracles/tags_read.rs` is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/tags_read.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
