//! Coverage-guided checks of the raises a plugin sends on its `NOTIFY`
//! binding (`bookclerk-plugin-abi::notifications::check_raise_params`), as
//! the workerd bridge's `/notify/raise` JSON body and as the Cap'n Proto
//! message a native guest sends. The oracle in
//! `fuzz/oracles/notify_raise.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/notify_raise.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
