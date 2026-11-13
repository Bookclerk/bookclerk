//! Coverage-guided validation of storefront fetch replies
//! (`bookclerk-plugin-abi::validate_plain_fetch`). A reply is a jailed
//! guest's `ContentSource.fetchTitle` answer, and the host validates it
//! before acquiring any file it names. The oracle in
//! `fuzz/oracles/plain_fetch.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/plain_fetch.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
