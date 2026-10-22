//! Coverage-guided peer input to the multiplexed byte link
//! (`bookclerk_plugin_sdk::mux`): the framing every native guest shares with
//! `bookclerk-workerd`, and so with the host.
//!
//! The input is the peer's whole byte stream, fed to a server-side `Mux`
//! whose local side opens one stream, accepts every peer stream and drains
//! it. The oracle in `fuzz/oracles/mux_peer.rs` (shared with the replay test
//! in `bookclerk-plugin-sdk/tests/fuzz_corpus.rs`) checks that the mux
//! settles once the bytes end, delivers no more than the peer sent, accepts
//! no more streams than the peer could open, and leaves no task behind
//! (`docs/adr/untrusted-input-fuzzing.md`).

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/mux_peer.rs"]
#[allow(dead_code)]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
