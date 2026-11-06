//! Coverage-guided parsing of Libro.fm catalog answers
//! (`bookclerk-plugin-source-libro`: `explore/search` pages and
//! `explore/audiobook_details` bodies). The store controls these bytes, and
//! the records they become reach Discover, purchase links and the demo seed.
//! The oracle in `fuzz/oracles/libro_catalog.rs` is shared with the replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/libro_catalog.rs"]
mod oracle;

/// Largest input worth parsing; a real details body is about 20 KiB.
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
