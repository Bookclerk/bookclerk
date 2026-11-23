//! Coverage-guided fuzzing of the Cloudflare API replies the `r2` guest's
//! guided setup reads (`bookclerk-storage::cloudflare_api`). The oracle in
//! `fuzz/oracles/cloudflare_api.rs` is shared with the crate's corpus
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cloudflare_api.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
