//! Coverage-guided classification of store refusal text
//! (`bookclerk-storefront::is_throttle_reason`). The store controls the
//! text, and a refusal read as a throttle pauses the whole account. The
//! oracle in `fuzz/oracles/throttle_reason.rs` is shared with the replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/throttle_reason.rs"]
mod oracle;

/// Largest input worth classifying; refusal messages are a sentence or two.
const MAX_INPUT_BYTES: usize = 4 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
