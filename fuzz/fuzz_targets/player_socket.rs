//! Coverage-guided checks of the host's `/player` API boundary named in
//! `fuzz/oracles/player_socket.rs`, whose rules the oracle states; the oracle is
//! shared with the `bookclerk-player-api` corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/player_socket.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
