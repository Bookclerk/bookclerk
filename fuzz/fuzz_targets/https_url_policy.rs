//! Coverage-guided checks of URLs the host fetches on a plugin's behalf
//! (`bookclerk-plugin-manifest::https_url_on_hosts`): a metadata provider's
//! cover URL, which the acquire overlay downloads outside the guest's own
//! egress policy. The oracle in `fuzz/oracles/https_url_policy.rs` is shared
//! with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/https_url_policy.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
