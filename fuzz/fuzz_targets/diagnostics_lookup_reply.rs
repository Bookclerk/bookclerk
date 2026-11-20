//! Coverage-guided reading of the diagnostics collector's answers to a
//! report lookup and deletion (`bookclerk-config::parse_lookup_reply` and
//! `parse_delete_reply`), which the daemon shows to the operator. The
//! oracle in `fuzz/oracles/diagnostics_lookup_reply.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/diagnostics_lookup_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
