//! Coverage-guided reading of a library's extra copies, the `copies`
//! array of the `core.output` document (`PUT
//! /api/config/domains/core.output` and the stored row), with
//! `bookclerk-config::validate_copies`. The oracle in
//! `fuzz/oracles/output_copies.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/output_copies.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
