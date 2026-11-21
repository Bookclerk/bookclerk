//! Coverage-guided derivation of the network destination a plugin setting's
//! value names (`bookclerk-plugin-manifest::derive_setting_destination`). The
//! oracle in `fuzz/oracles/setting_destination.rs` is shared with the replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/setting_destination.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
