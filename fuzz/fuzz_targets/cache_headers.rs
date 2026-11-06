//! Coverage-guided fuzzing of HTTP cache validators
//! (`bookclerk-cover::http_cache`): the `If-None-Match` header a client
//! sends for an image, which decides whether the daemon answers `304`. The
//! oracle in `fuzz/oracles/cache_headers.rs` is shared with the crate's
//! corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cache_headers.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
