//! Coverage-guided parsing of workerd `--control-fd` lines
//! (`bookclerk-workerd::control::listen_port`). On Windows the launcher reads
//! the bridge RPC port from workerd's stdout, which plugin isolates also write
//! to. The oracle in `fuzz/oracles/workerd_control.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/workerd_control.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
