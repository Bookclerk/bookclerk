//! Coverage-guided checks of a library card's token answer on the clerk
//! (`bookclerk-node-proto::parse_clerk_token_answer`): the body the
//! library sends back from `POST /api/clerk/v1/token`. The oracle in
//! `fuzz/oracles/clerk_token_answer.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/clerk_token_answer.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
