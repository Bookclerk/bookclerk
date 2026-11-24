//! Coverage-guided checks of what a notifier hands back to the host
//! (`bookclerk-plugin-abi::notifications`): a jailed guest's
//! `Notifier.send` reply, which decides whether a delivery is retried,
//! its `checkDeliverability` report, which the operator reads, and the
//! address documents a `validateDestination` answer or a patron's form
//! carries. The oracle in `fuzz/oracles/notifier_reply.rs`
//! is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/notifier_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
