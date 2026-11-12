//! Coverage-guided parsing of GraphicAudio product pages
//! (`bookclerk-plugin-source-graphicaudio`: `parse_product_detail`). The
//! store controls these bytes, and the record they become reaches title
//! dialogs, the listing pool and the demo seed. The oracle in
//! `fuzz/oracles/graphicaudio_catalog.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/graphicaudio_catalog.rs"]
mod oracle;

/// Largest input worth parsing; the parts of a product page the parser
/// reads are about 10 KiB (a whole page is about 320 KiB).
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
