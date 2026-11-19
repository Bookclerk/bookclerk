//! Coverage-guided checks of a destination id as another node sends it
//! (`bookclerk-config::DestinationId`): an acquire plan's destination and
//! upload records name the owner's destinations by it. The oracle in
//! `fuzz/oracles/destination_id.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/destination_id.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
