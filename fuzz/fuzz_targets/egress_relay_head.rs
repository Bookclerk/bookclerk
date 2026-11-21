//! Coverage-guided parsing of the heads the launcher's egress relay reads:
//! request heads from workerd (plugin code) and response heads from the
//! servers the plugin reached. The oracle in
//! `fuzz/oracles/egress_relay_head.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/egress_relay_head.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
