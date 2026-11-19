//! Coverage-guided validation of host-brokered socket messages
//! (`bookclerk-plugin-abi::sockets`): a jailed guest's `Sockets.connect`
//! request and URL, and the `SocketMessage` the host delivers to a guest's
//! `SocketHandler`. The host checks both before anything reaches the broker
//! or the guest. The oracle in `fuzz/oracles/socket_request.rs` is shared
//! with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/socket_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
