//! Coverage-guided parsing of a workerd isolate's `env.SOCKETS` calls on the
//! granted reverse channel (`bookclerk-workerd::granted_sockets`): the
//! `POST /sockets/<op>` JSON body for `connect`, `send`, `close`,
//! `attachment` and `list`, every byte of which a plugin isolate controls.
//! The oracle in `fuzz/oracles/socket_route.rs` is shared with the replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/socket_route.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
