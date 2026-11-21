//! Coverage-guided checks of the chapter route bodies
//! (`bookclerk-library::parse_chapter_edit_request` and
//! `parse_chapter_pin_request`): the bodies of
//! `PUT /api/library/editions/{id}/chapters` and
//! `PUT /api/library/editions/{id}/chapters/pin`. The oracle in
//! `fuzz/oracles/chapter_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/chapter_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
