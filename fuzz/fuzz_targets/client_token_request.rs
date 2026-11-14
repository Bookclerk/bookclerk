//! Coverage-guided checks of a library card's token exchange
//! (`bookclerk-node-proto::parse_client_token_request` and
//! `client_exchange_matches`): the body of `POST /api/client/v1/token`
//! and its match against the stored code. The oracle in
//! `fuzz/oracles/client_token_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/client_token_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
