//! Coverage-guided check of the socket broker's bookkeeping
//! (`bookclerk-workerd::socket_table`): scripts of connect requests and
//! frames a guest sends, frames and closes a remote WebSocket server sends,
//! and handshake outcomes, against an independent model. The oracle in
//! `fuzz/oracles/socket_broker.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/socket_broker.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
