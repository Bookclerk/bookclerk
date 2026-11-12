//! Coverage-guided checks of a branch's job cap request
//! (`bookclerk-library::control_plane::parse_job_cap_request`): the body of
//! `POST /api/config/join/capacity`. The oracle in
//! `fuzz/oracles/job_cap_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/job_cap_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
