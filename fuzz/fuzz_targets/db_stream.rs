//! Coverage-guided decoding of adapter-produced `executeStreamed` frames
//! (`bookclerk-plugin-abi::ExecuteStreamDecoder`). The oracle in
//! `fuzz/oracles/db_stream.rs` is shared with the `bookclerk-plugin-abi`
//! corpus replay test: one push and chunked pushes agree, an accepted reply
//! stays within the budget, and it round-trips through the encoder.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/db_stream.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
