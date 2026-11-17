//! Coverage-guided decoding and validation of a storage guest's replies to
//! intent operations (`bookclerk-plugin-abi::decode_storage_reply_bytes` and
//! the `validate_*` checks the `DestinationClient` runs). A reply comes from
//! a jailed guest and the host acts on it, so every page is checked against
//! the request before it leaves the ABI client. The oracle in
//! `fuzz/oracles/storage_reply.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/storage_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
