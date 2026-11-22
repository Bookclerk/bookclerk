//! Coverage-guided fuzzing of what the `b2` destination reads from
//! Backblaze B2: native API replies and download headers
//! (`bookclerk-storage::b2_api`). The oracle in `fuzz/oracles/b2_reply.rs`
//! is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/b2_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
