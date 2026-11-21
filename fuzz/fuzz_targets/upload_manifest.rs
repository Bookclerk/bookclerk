//! Coverage-guided fuzzing of the upload manifest and `Upload-Offset`
//! parsers. The oracle in `fuzz/oracles/upload_manifest.rs` is shared with
//! the `bookclerk-library` corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/upload_manifest.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
