//! Coverage-guided `cache.*` settings parsing
//! (`bookclerk-config::parse_cache_setting`). The values come from
//! `PATCH /api/settings` and `bookclerk config set`. The oracle in
//! `fuzz/oracles/cache_settings.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cache_settings.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
