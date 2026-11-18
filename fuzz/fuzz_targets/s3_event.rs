//! Coverage-guided fuzzing of the S3 notification messages the `s3`
//! destination drains from its bucket's queue
//! (`bookclerk-storage::s3_events::parse_event_message`). The oracle in
//! `fuzz/oracles/s3_event.rs` is shared with the crate's corpus replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/s3_event.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
