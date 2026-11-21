//! Coverage-guided checks of one request on the public webhook route
//! (`POST /hooks/<token>/<name>`, `bookclerk-workerd::webhook_request`):
//! the body, the headers and the HMAC-SHA256 signature anyone on the
//! Internet sends. The oracle in `fuzz/oracles/webhook_request.rs` is shared
//! with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/webhook_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
