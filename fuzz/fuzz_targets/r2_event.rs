//! Coverage-guided fuzzing of what the `r2` destination reads from
//! Cloudflare: R2 event notifications and Queues pull replies
//! (`bookclerk-storage::r2_events`). The oracle in `fuzz/oracles/r2_event.rs`
//! is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/r2_event.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
