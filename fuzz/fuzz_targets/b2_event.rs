//! Coverage-guided fuzzing of what the `b2` destination's change feed reads from
//! B2: webhook bodies of event notifications and notification-rule replies
//! (`bookclerk-storage::b2_events`). The oracle in `fuzz/oracles/b2_event.rs`
//! is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/b2_event.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
