//! Coverage-guided checks of a storage destination's filter as an operator
//! sends it (`bookclerk-config::parse_destination_filter`): the body of
//! `PUT /api/config/plugin-instances/{id}/filter` and the stored
//! `core.destination` document. The oracle in
//! `fuzz/oracles/destination_filter.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/destination_filter.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
