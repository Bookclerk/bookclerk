//! Coverage-guided validation of storefront scan pages
//! (`bookclerk-plugin-abi::validate_scan_page`). A page is a jailed guest's
//! `ContentSource.scan` reply, and the host validates it before writing any
//! title. The oracle in `fuzz/oracles/scan_page.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/scan_page.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
