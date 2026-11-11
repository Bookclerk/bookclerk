//! Coverage-guided validation of guest `EVENTS.read` requests
//! (`bookclerk-plugin-abi::parse_event_read_json`,
//! `validate_event_read_request` and `EventCursor::decode`). A request is a
//! jailed guest's ask to read past outbox events, and the host checks it
//! before it touches the outbox. The oracle in `fuzz/oracles/event_read.rs`
//! is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/event_read.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
