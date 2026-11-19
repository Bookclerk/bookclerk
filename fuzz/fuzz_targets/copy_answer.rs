//! Coverage-guided checks of a copy download's answer as a reader
//! receives it (`bookclerk-node-proto::copy_answer_head_end` and
//! `parse_copy_answer`): where its head ends, however the bytes were cut
//! into reads, and what the head says of the bytes that follow, which
//! decides how many a reader takes from the library it took a copy from
//! and where in the file it writes them. The oracle in
//! `fuzz/oracles/copy_answer.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/copy_answer.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
