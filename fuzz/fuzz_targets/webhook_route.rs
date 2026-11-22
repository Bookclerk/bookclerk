//! Coverage-guided parsing of a workerd isolate's `env.WEBHOOKS` calls on
//! the granted reverse channel (`bookclerk-workerd::granted_webhooks`): the
//! `POST /webhooks/<op>` JSON body for `url`, `pull` and `ack`, every byte
//! of which a plugin isolate controls. The oracle in
//! `fuzz/oracles/webhook_route.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/webhook_route.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
