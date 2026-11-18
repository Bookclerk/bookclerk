//! Coverage-guided checks of the head of a copy download's answer
//! (`bookclerk-node-proto::parse_copy_answer`), which decides how many
//! bytes a reader takes from the library it took a copy from and where in
//! the file it writes them. The oracle in `fuzz/oracles/copy_answer.rs` is
//! shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/copy_answer.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
