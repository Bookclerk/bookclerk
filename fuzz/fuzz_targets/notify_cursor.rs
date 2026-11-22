//! Coverage-guided checks of a notifications page cursor
//! (`bookclerk-library::NotifyCursor::decode`): the `cursor` query
//! parameter of the delivery log and the inbox. The oracle in
//! `fuzz/oracles/notify_cursor.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/notify_cursor.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
