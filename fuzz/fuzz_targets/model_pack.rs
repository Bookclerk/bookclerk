//! Coverage-guided fuzzing of the embedding model directory readers
//! (`bookclerk-model-pack`): the model description and the row packs. The
//! oracle in `fuzz/oracles/model_pack.rs` is shared with the crate's corpus
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/model_pack.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
