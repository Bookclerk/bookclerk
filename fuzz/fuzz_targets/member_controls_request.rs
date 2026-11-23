//! Coverage-guided checks of a member controls body
//! (`bookclerk-library::control_plane::parse_default_controls_patch` and
//! `parse_user_controls_patch`): the bodies of
//! `PATCH /api/users/default-controls` and `PATCH /api/users/{id}/controls`.
//! The oracle in `fuzz/oracles/member_controls_request.rs` is shared with
//! the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/member_controls_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
