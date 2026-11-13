//! Coverage-guided checks of an interlibrary request body
//! (`bookclerk-library::control_plane::parse_branch_request`): the body of
//! `POST /api/branch-requests`. The oracle in
//! `fuzz/oracles/branch_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/branch_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
