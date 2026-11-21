//! Coverage-guided parsing of relayed helper and guest stderr lines
//! (`bookclerk-config::parse_relayed_line`). A jailed process writes these
//! bytes, and the level the parser reads decides how the host logs them, so
//! a guest must not be able to raise a line above WARN. The oracle in
//! `fuzz/oracles/relayed_log_line.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/relayed_log_line.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
