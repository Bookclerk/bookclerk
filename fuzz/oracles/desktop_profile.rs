//! Shared oracle for the `desktop_profile` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the desktop app's `profile.toml`, a file in the OS
//! user's config directory that anything running as that user can edit,
//! read by [`parse_profile`] when the window starts. The invariants,
//! restated from the TOML table rather than read from the parser:
//!
//! - **Acceptance.** A file is accepted exactly when it is at most 4096
//!   bytes of UTF-8 TOML whose table has the keys `version`, `mode` and
//!   `session` and no others; `version` is the integer 1, `mode` is the
//!   string `branch`, and `session` is a string of 36 characters with `-`
//!   at offsets 8, 13, 18 and 23 and a lowercase hex digit everywhere else.
//! - **Value.** An accepted profile carries that session text, byte for
//!   byte.
//! - **Round trip.** Writing an accepted profile and parsing the text again
//!   gives an equal profile.

use bookclerk_config::{parse_profile, ProfileMode};

/// Largest input worth checking (a little past the parser's own limit, so
/// the size refusal is exercised).
pub const MAX_INPUT_BYTES: usize = 4200;

/// Whether `text` has the session token's shape, checked character by
/// character.
fn token_shaped(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                matches!(c, '0'..='9' | 'a'..='f')
            }
        })
}

/// The session text the oracle expects the parser to accept, or `None`
/// when it expects a refusal.
fn expected(data: &[u8]) -> Option<String> {
    if data.len() > 4096 {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let table: toml::Table = toml::from_str(text).ok()?;
    let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != ["mode", "session", "version"] {
        return None;
    }
    if table.get("version")?.as_integer()? != 1 {
        return None;
    }
    if table.get("mode")?.as_str()? != "branch" {
        return None;
    }
    let session = table.get("session")?.as_str()?;
    token_shaped(session).then(|| session.to_owned())
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The profile file's bytes.
///
/// # Returns
///
/// Whether the parser accepted the input.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let parsed = parse_profile(data);
    match (expected(data), &parsed) {
        (Some(session), Ok(profile)) => {
            assert_eq!(profile.session.expose(), session);
            assert_eq!(profile.mode, ProfileMode::Branch);
            assert_eq!(
                parse_profile(profile.to_toml().as_bytes()).as_ref(),
                Ok(profile)
            );
            true
        }
        (None, Err(_)) => false,
        (want, got) => panic!("oracle expected {:?}, parser gave {got:?}", want.is_some()),
    }
}
