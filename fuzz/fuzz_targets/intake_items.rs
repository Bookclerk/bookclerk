//! Coverage-guided checks of how a drop folder's listing becomes items and
//! when each is ready (`bookclerk-intake::ItemGrouper` and `step`). Keys
//! come from a folder anyone with access can fill. The oracle in
//! `fuzz/oracles/intake_items.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/intake_items.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
