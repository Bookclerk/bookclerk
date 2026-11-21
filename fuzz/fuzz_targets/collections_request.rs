//! Coverage-guided checks of a collections body
//! (`bookclerk-library::control_plane::parse_collections_request`): the body of
//! `PUT /api/library/collections`. The oracle in
//! `fuzz/oracles/collections_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/collections_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
