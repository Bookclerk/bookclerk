//! Coverage-guided checks of the `address` object in a patron's
//! `POST /api/me/notifications/destinations` body
//! (`bookclerk-plugin-abi::notifications::AddressFields`). The oracle in
//! `fuzz/oracles/notify_address_body.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/notify_address_body.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
