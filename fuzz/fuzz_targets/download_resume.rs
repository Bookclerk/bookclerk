//! Coverage-guided checks of how a guest resumes a store download
//! (`bookclerk-plugin-sdk::resume`): the CDN's `Content-Range` and status
//! decide whether its body is appended to the bytes already on disk,
//! replaces them, or ends the download. The oracle in
//! `fuzz/oracles/download_resume.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/download_resume.rs"]
mod oracle;

/// Largest input worth checking: the fixed head plus a header well past
/// the parser's length cap.
const MAX_INPUT_BYTES: usize = 512;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let _ = oracle::check(data);
});
