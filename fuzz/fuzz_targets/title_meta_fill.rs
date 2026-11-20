//! Coverage-guided checks of a title-meta query
//! (`bookclerk-discover::check_title_meta_query` and
//! `plan_title_meta_fill`): the body of `POST /api/discover/title-meta` and
//! each batch entry, and the storefront lookups it plans. The oracle in
//! `fuzz/oracles/title_meta_fill.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/title_meta_fill.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
