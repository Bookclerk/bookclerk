//! Coverage-guided fuzzing of `bookclerk-archive`. The oracle in
//! `fuzz/oracles/archive_report.rs` is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/archive_report.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
