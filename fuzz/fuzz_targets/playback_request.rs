//! Coverage-guided checks of what a player sends the playback routes
//! (`bookclerk-library::parse_playback_sync_request`,
//! `parse_open_session_request`, `parse_progress_edit_request`,
//! `parse_new_bookmark_request`, `parse_bookmark_edit_request`,
//! `parse_progress_cursor` and `parse_track_path`): the bodies that open
//! a session, report or set progress and add or change a bookmark, the
//! progress page cursor, and the path of
//! `GET /api/playback/sessions/{id}/tracks/{n}`. The oracle in
//! `fuzz/oracles/playback_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/playback_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
