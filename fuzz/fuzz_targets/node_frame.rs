//! Coverage-guided parsing of node-to-node frames
//! (`bookclerk-node-proto::Frame`). A frame arrives from another node of the
//! cluster, and its `acquire.targets` library ids name storage keys on a
//! host. The oracle in `fuzz/oracles/node_frame.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/node_frame.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
