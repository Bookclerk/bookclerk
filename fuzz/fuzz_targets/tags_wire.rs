//! Coverage-guided fuzzing of the tag worker frames (`bookclerk-tags::wire`):
//! requests the worker reads and replies the host reads. The oracle in
//! `fuzz/oracles/tags_wire.rs` is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/tags_wire.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
