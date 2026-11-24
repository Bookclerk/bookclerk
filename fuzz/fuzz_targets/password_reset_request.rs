//! Coverage-guided checks of the self-service password-reset bodies
//! (`bookclerk-library::notifications::parse_password_reset_ask`,
//! `parse_password_reset_link` and `parse_password_reset_finish`): the
//! bodies of `POST /api/auth/password-reset`, `/check`, `/passkey` and
//! `/finish`. The oracle in `fuzz/oracles/password_reset_request.rs` is
//! shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/password_reset_request.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
