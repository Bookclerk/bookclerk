//! Coverage-guided checks of a library tags body
//! (`bookclerk-library::parse_work_tags_request`): the body of
//! `PUT /api/library/works/{id}/tags`. The oracle in
//! `fuzz/oracles/work_tags_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/work_tags_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
