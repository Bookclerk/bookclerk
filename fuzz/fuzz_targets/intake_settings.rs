//! Coverage-guided checks of a drop folder's settings as an operator sends
//! them (`bookclerk-config::parse_intake_settings`): the body of
//! `PUT /api/config/plugin-instances/{id}/intake` and the stored
//! `core.intake` document. The oracle in `fuzz/oracles/intake_settings.rs`
//! is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/intake_settings.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
