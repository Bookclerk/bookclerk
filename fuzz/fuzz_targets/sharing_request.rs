//! Coverage-guided checks of a branch's sharing settings request
//! (`bookclerk-library::control_plane::parse_sharing_request`): the body of
//! `POST /api/config/join/sharing`. The oracle in
//! `fuzz/oracles/sharing_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/sharing_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
