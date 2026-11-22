//! Coverage-guided checks of what a model plugin answers on its `ai`
//! entrypoint: the reply to a run
//! (`bookclerk-plugin-abi::ai::check_run_output`) and the model listing
//! (`bookclerk-plugin-abi::ai::check_models`). The oracle in
//! `fuzz/oracles/ai_run_reply.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/ai_run_reply.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
