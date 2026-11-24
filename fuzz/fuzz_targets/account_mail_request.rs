//! Coverage-guided checks of the account-mail request bodies
//! (`bookclerk-library::notifications::parse_account_mail_settings` and
//! `parse_verify_email_request`): the bodies of
//! `PUT /api/notifications/account-mail` and `POST /api/auth/verify-email`.
//! The oracle in `fuzz/oracles/account_mail_request.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/account_mail_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
