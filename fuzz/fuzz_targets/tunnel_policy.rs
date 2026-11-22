//! Coverage-guided checks of what a request on the `tunnel` transport may
//! reach (`bookclerk-node-proto::tunnel_refusal`): the operator-only paths,
//! the branches exposure and the router's path normalization. The oracle in
//! `fuzz/oracles/tunnel_policy.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/tunnel_policy.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
