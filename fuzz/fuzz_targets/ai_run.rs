//! Coverage-guided checks of the runs a plugin sends on its `AI` binding
//! (`bookclerk-plugin-abi::ai::check_binding_run_params`), as the workerd
//! bridge's `/ai/run` JSON body and as the Cap'n Proto message a native
//! guest sends. The oracle in `fuzz/oracles/ai_run.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/ai_run.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
