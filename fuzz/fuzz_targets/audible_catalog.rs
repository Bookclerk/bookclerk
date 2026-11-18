//! Coverage-guided parsing of Audible public catalog answers
//! (`bookclerk-audible-catalog`: `/1.0/catalog/products`, `/1.0/catalog/search`
//! and `/1.0/catalog/products/{asin}/sims` bodies). The store controls these
//! bytes, and the titles they become reach Discover candidates, their genres
//! and host enrichment. The oracle in `fuzz/oracles/audible_catalog.rs` is
//! shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/audible_catalog.rs"]
mod oracle;

/// Largest input worth parsing; a 50-title answer is about 330 KiB, so this
/// keeps a few titles' worth.
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
