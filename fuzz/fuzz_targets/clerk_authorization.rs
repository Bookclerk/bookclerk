//! Coverage-guided checks of a clerk's `Authorization` header
//! (`bookclerk-node-proto::parse_clerk_authorization`): the library card a
//! clerk signs in with. The oracle in
//! `fuzz/oracles/clerk_authorization.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/clerk_authorization.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
