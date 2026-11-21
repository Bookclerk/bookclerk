//! Coverage-guided validation of what a relay `ingress` plugin hands the
//! host (`bookclerk-plugin-abi::origin`): the framed `POST /origin/fetch`
//! body on the isolate bridge, request and response heads, and the public
//! origin `Ingress.start` reports. The oracle in
//! `fuzz/oracles/origin_request.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/origin_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
