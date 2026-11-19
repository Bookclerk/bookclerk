//! Coverage-guided checks of a patron scope body
//! (`bookclerk-library::control_plane::parse_patron_scope` and
//! `parse_user_scope_request`): the bodies of `PUT /api/users/default-scope`
//! and `PUT /api/users/{id}/scope`. The oracle in
//! `fuzz/oracles/patron_scope_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/patron_scope_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
