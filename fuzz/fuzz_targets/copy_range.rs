//! Coverage-guided checks of the `Range` header of a copy download
//! (`bookclerk-node-proto::parse_copy_range`), which decides which bytes
//! of a library's file a holder of a copy token is sent. The oracle in
//! `fuzz/oracles/copy_range.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/copy_range.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
