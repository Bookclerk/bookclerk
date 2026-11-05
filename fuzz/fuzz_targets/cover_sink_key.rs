//! Coverage-guided cover sink key parsing
//! (`bookclerk-plugin-abi::cover_sink_slot`). A cover fetcher guest picks
//! the key each image body is stored under, and the host maps it to a URL
//! slot. The oracle in `fuzz/oracles/cover_sink_key.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cover_sink_key.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
