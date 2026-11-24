//! Coverage-guided reading of Chirp's title detail answers
//! (`bookclerk-plugin-source-chirp`: `audiobook_detail_record`). The store
//! controls these bytes, and the record they become reaches title dialogs
//! and names the sample the host downloads. The oracle in
//! `fuzz/oracles/chirp_catalog_detail.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/chirp_catalog_detail.rs"]
mod oracle;

/// Largest input worth parsing; a detail answer is about 4 KiB.
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
