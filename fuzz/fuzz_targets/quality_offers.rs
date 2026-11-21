//! Coverage-guided validation of storefront quality offers
//! (`bookclerk-plugin-abi::validate_quality_offers`). An offer list is a
//! jailed guest's `ContentSource.qualityOffers` reply, and the host
//! validates it before comparing any offer with a held file. The oracle in
//! `fuzz/oracles/quality_offers.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/quality_offers.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
