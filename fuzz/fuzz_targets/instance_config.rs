//! Coverage-guided plugin instance settings: the `settings` object of
//! `PUT /api/config/plugin-instances/{id}/config` checked by
//! `bookclerk-library::control_plane::PluginInstanceConfigV1::validate`,
//! and the table row text of `bookclerk plugins config set`
//! (`parse_setting_text`). The oracle in `fuzz/oracles/instance_config.rs`
//! is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/instance_config.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
