//! Coverage-guided checks of a branch's store-connection requests
//! (`bookclerk-library::control_plane::parse_add_connections_request` and
//! `parse_disconnect_request`): the bodies of
//! `POST /api/config/join/connections` and
//! `POST /api/config/join/connections/disconnect`. The oracle in
//! `fuzz/oracles/connections_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/connections_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
