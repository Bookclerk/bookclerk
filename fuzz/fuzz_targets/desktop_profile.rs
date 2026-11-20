//! Coverage-guided parsing of the desktop app's per-OS-user `profile.toml`
//! (`bookclerk-config::parse_profile`), a file anything running as that OS
//! user can edit. The oracle in `fuzz/oracles/desktop_profile.rs` is shared
//! with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/desktop_profile.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
