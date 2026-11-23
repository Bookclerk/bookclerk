//! Coverage-guided fuzzing of what the host parses while it downloads an
//! embedding model (`bookclerk-plugin-catalog::model_fetch`): a registry's
//! token reply and a plugin package's model list. The oracle in
//! `fuzz/oracles/model_fetch.rs` is shared with the crate's corpus replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/model_fetch.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
