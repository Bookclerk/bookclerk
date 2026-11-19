//! Coverage-guided checks of the clerk socket's frames in both
//! directions (`bookclerk-node-proto::parse_clerk_frame` and
//! `parse_library_frame`): what a library reads from a clerk on
//! `GET /api/clerk/v1/connect`, and what a clerk reads back. The oracle
//! in `fuzz/oracles/clerk_socket_frame.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/clerk_socket_frame.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
