//! Coverage-guided checks of the scan and acquire planners acting for a
//! Member (`bookclerk-library::control_plane::plan_scan` / `plan_acquire`):
//! every planned job reaches only store accounts linked to that Member. The
//! oracle in `fuzz/oracles/plan_authz.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/plan_authz.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
