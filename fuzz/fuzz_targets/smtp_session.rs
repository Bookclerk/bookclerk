//! Coverage-guided SMTP client checks (`bookclerk-plugin-notifier-email`:
//! `read_reply`, `Session`, `parse_address`, `parse_recipients`). A mail
//! server controls the replies the email guest reads while it holds a
//! patron's address, account-mail links and the operator's SMTP password;
//! a patron controls the address document. The oracle in
//! `fuzz/oracles/smtp_session.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/smtp_session.rs"]
mod oracle;

/// Largest input worth checking: a long EHLO answer and a session's
/// replies fit well within it.
const MAX_INPUT_BYTES: usize = 32 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    oracle::check(data);
});
