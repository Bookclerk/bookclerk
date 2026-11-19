//! Coverage-guided checks of a work split body
//! (`bookclerk-library::parse_work_split_request`): the body of
//! `POST /api/library/works/{id}/split`. The oracle in
//! `fuzz/oracles/work_split_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/work_split_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
