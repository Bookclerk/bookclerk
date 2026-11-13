//! Coverage-guided checks of the desktop window's custom-scheme policy
//! (`bookclerk-window-policy`): which page requests reach the router with
//! the operator token, the headers the router then sees, where the window
//! may navigate, and how long a join's sign-in stays open. The oracle in
//! `fuzz/oracles/window_policy.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/window_policy.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
