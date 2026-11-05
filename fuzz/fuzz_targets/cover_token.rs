//! Coverage-guided fuzzing of cover tokens (`bookclerk-cover::token`): the
//! signed path segment the cover route turns into the remote URL it may
//! fetch. The oracle in `fuzz/oracles/cover_token.rs` is shared with the
//! crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cover_token.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
