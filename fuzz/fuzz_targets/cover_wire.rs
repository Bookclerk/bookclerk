//! Coverage-guided fuzzing of the cover worker frames
//! (`bookclerk-cover::wire`): requests the worker reads and replies the host
//! reads. The oracle in `fuzz/oracles/cover_wire.rs` is shared with the
//! crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cover_wire.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
