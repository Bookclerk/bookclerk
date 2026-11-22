//! Coverage-guided decoding of the Cap'n Proto `Webhooks` messages a native
//! guest sends the host (`bookclerk-plugin-abi`): pull and ack requests,
//! and deliveries. The oracle in `fuzz/oracles/webhook_message.rs` is shared
//! with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/webhook_message.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
