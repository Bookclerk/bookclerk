//! Coverage-guided checks of a members' books choice
//! (`bookclerk-library::control_plane::parse_members_books_request`): the
//! body of `PUT /api/config/branches/members-books`. The oracle in
//! `fuzz/oracles/members_books_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/members_books_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
