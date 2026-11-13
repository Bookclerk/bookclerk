//! Coverage-guided checks of a client app's authorization request
//! (`bookclerk-node-proto::parse_client_query` and
//! `client_request_problem`): the query `/oidc/authorize` turns into the
//! consent page for a library card. The oracle in
//! `fuzz/oracles/client_authorize.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/client_authorize.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
