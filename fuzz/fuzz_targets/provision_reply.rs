//! Coverage-guided fuzzing of a plugin's answer to its guided setup
//! (`Provisioning.provision`), decoded and checked by the host against the
//! declared outputs (`bookclerk-plugin-abi::provision`). The oracle in
//! `fuzz/oracles/provision_reply.rs` is shared with the crate's corpus
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/provision_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
