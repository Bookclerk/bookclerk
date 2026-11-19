//! Coverage-guided reading of the diagnostics collector's `POST /submit`
//! reply (`bookclerk-config::parse_submit_reply`), whose `report_id` joins
//! the install's report history. The oracle in
//! `fuzz/oracles/diagnostics_submit_reply.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/diagnostics_submit_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
