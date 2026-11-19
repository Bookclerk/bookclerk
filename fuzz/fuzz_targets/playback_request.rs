//! Coverage-guided checks of what a player sends the playback routes
//! (`bookclerk-library::parse_playback_sync_request` and
//! `parse_track_path`): the body of
//! `POST /api/playback/sessions/{id}/sync` and `.../close`, and the path
//! of `GET /api/playback/sessions/{id}/tracks/{n}`. The oracle in
//! `fuzz/oracles/playback_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/playback_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
