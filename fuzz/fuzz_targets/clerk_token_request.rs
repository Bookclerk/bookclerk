//! Coverage-guided checks of a library card's token exchange
//! (`bookclerk-node-proto::parse_clerk_token_request` and
//! `clerk_exchange_matches`): the body of `POST /api/clerk/v1/token`
//! and its match against the stored code. The oracle in
//! `fuzz/oracles/clerk_token_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/clerk_token_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
