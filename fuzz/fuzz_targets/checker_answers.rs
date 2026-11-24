//! Coverage-guided checks of what the `email` guest's deliverability check
//! reads from outside services (`bookclerk-plugin-notifier-email::
//! deliverability`): Postmark's spam check, Cloudflare's DNS-over-HTTPS
//! answers and mail-tester results, which the host does not produce. The
//! oracle in `fuzz/oracles/checker_answers.rs` is shared with the replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/checker_answers.rs"]
mod oracle;

/// Largest input worth checking: the guest reads at most 256 KiB of an
/// answer, and a few KiB cover every shape.
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
