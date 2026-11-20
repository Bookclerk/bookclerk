//! Coverage-guided checks of a store account's pace change request
//! (`bookclerk-library::pacing::parse_pace_request`): the body of
//! `POST /api/portal/sources/{source}/connections/{account_id}/pace`. The
//! oracle in `fuzz/oracles/pace_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/pace_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
