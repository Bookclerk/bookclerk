//! Coverage-guided fuzzing of the media worker's keep-codec M4B packager
//! (`bookclerk-media::package_keeping_codec`) over one untrusted audio
//! part. The oracle in `fuzz/oracles/media_keep_codec.rs` is shared with
//! the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/media_keep_codec.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _kept = oracle::check(data);
});
