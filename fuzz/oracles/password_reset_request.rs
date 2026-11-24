//! Shared oracle for the `password_reset_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the three bodies anyone who can reach the library sends
//! to self-service password reset: the address to mail a link to
//! (`POST /api/auth/password-reset`, [`parse_password_reset_ask`]), a
//! link's token (`POST /api/auth/password-reset/check` and `/passkey`,
//! [`parse_password_reset_link`]) and the finish body
//! (`POST /api/auth/password-reset/finish`,
//! [`parse_password_reset_finish`]). The input's first byte picks the body
//! (modulo 3: ask, link, finish); the rest is the body.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value: every body must be one JSON object with no
//! repeated keys and no keys but its own, or it is refused.
//!
//! - Ask: the only key is `email`, a string. An accepted address equals
//!   the string trimmed and ASCII-lowercased, is at most 254 bytes and has
//!   a dotted domain after its last `@`; a plain address (letters, digits
//!   and `._+-` before the `@`, dotted letter-and-digit labels after it)
//!   is always accepted.
//! - Link: the only key is `token`, exactly 64 lowercase hex characters.
//! - Finish: `token` (as above) and `password` (1 to 1024 bytes once
//!   trimmed) are required; `totp_code` (six ASCII digits) and `passkey`
//!   (an object of exactly `challenge_id`, 1 to 64 printable ASCII bytes,
//!   and `credential`, an object) and `recovery_code` (at most 40 bytes
//!   that, ignoring spaces and hyphens and case, and reading `I` and `L`
//!   as `1` and `O` as `0`, are exactly 16 characters of Crockford's
//!   base32 alphabet) are optional, `null` counting as absent, and at
//!   most one of them is sent. The parsed values equal the ones written.
//!
//! A refused body is `Invalid`, never another error.

use bookclerk_library::notifications::{
    parse_password_reset_ask, parse_password_reset_finish, parse_password_reset_link,
    ResetSecondFactor,
};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking; a real body is well under 4 KiB.
pub const MAX_INPUT_BYTES: usize = 4096;

/// What an address may be at most, in bytes.
const EMAIL_MAX: usize = 254;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A selector byte, then the request body bytes.
///
/// # Returns
///
/// True when the parser accepted the body.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Some((selector, body)) = data.split_first() else {
        return false;
    };
    // The messages name the input and the verdict, never the parsed value,
    // so a failure prints only what the fuzzer fed in.
    let shown = String::from_utf8_lossy(body);
    match selector % 3 {
        0 => check_ask(body, &shown),
        1 => check_link(body, &shown),
        _ => check_finish(body, &shown),
    }
}

/// The ask body: a plain address must pass, and anything accepted must be
/// the normalized address and look like one.
fn check_ask(body: &[u8], shown: &str) -> bool {
    let got = parse_password_reset_ask(body);
    let Some(Value::String(raw)) =
        object_of(body, &["email"]).and_then(|f| f.get("email").cloned())
    else {
        assert!(
            matches!(got, Err(LibraryError::Invalid(_))),
            "{shown:?}: the parser took a body the oracle refuses"
        );
        return false;
    };
    let want = raw.trim().to_ascii_lowercase();
    match got {
        Ok(address) => {
            assert!(
                address == want && could_be_address(&want),
                "{shown:?}: the parser took an address the oracle refuses"
            );
            true
        }
        Err(LibraryError::Invalid(_)) => {
            assert!(
                !is_plain_address(&want),
                "{shown:?}: the parser refused a plain address"
            );
            false
        }
        Err(_) => panic!("{shown:?}: a refusal that is not Invalid"),
    }
}

/// The link body: one well-formed `token`.
fn check_link(body: &[u8], shown: &str) -> bool {
    let got = parse_password_reset_link(body);
    let want = object_of(body, &["token"])
        .filter(|f| f.len() == 1)
        .and_then(|f| f.get("token").cloned())
        .and_then(|v| link_token(&v));
    match want {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{shown:?}: the parser took a body the oracle refuses"
            );
            false
        }
        Some(want) => {
            assert!(
                got.is_ok_and(|token| token.expose() == want),
                "{shown:?}: the parser and the oracle disagree"
            );
            true
        }
    }
}

/// What a finish body must parse to: the token, the trimmed password and
/// the second factor.
#[derive(Debug, PartialEq)]
struct Finish {
    /// The link's token.
    token: String,
    /// The password, trimmed.
    password: String,
    /// The second factor.
    factor: Factor,
}

/// The second factor a finish body carries.
#[derive(Debug, PartialEq)]
enum Factor {
    /// None.
    None,
    /// An authenticator code.
    Totp(String),
    /// A passkey's challenge id and credential.
    Passkey(String, Value),
    /// A recovery code: its 16 symbols, normalized.
    Recovery(String),
}

/// The finish body.
fn check_finish(body: &[u8], shown: &str) -> bool {
    let got = parse_password_reset_finish(body);
    match expected_finish(body) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{shown:?}: the parser took a body the oracle refuses"
            );
            false
        }
        Some(want) => {
            let Ok(finish) = got else {
                panic!("{shown:?}: the parser refused a body the oracle takes");
            };
            let factor = match finish.second_factor {
                ResetSecondFactor::None => Factor::None,
                ResetSecondFactor::Totp(code) => Factor::Totp(code),
                ResetSecondFactor::Passkey {
                    challenge_id,
                    credential,
                } => Factor::Passkey(challenge_id, credential),
                ResetSecondFactor::RecoveryCode(code) => {
                    Factor::Recovery(code.display().replace('-', ""))
                }
            };
            let parsed = Finish {
                token: finish.token.expose().to_string(),
                password: finish.password.as_str().to_string(),
                factor,
            };
            assert!(
                parsed == want,
                "{shown:?}: the parser and the oracle disagree"
            );
            true
        }
    }
}

/// The finish body the oracle expects; `None` when it must be refused.
fn expected_finish(body: &[u8]) -> Option<Finish> {
    let fields = object_of(
        body,
        &["token", "password", "totp_code", "passkey", "recovery_code"],
    )?;
    let token = link_token(fields.get("token")?)?;
    let Value::String(password) = fields.get("password")? else {
        return None;
    };
    let password = password.trim();
    if password.is_empty() || password.len() > 1024 {
        return None;
    }
    let code = match fields.get("totp_code") {
        None | Some(Value::Null) => None,
        Some(Value::String(code))
            if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some(code.clone())
        }
        Some(_) => return None,
    };
    let passkey = match fields.get("passkey") {
        None | Some(Value::Null) => None,
        Some(Value::Object(answer)) => {
            if answer.len() != 2 || passkey_has_repeated_key(body) {
                return None;
            }
            let Some(Value::String(challenge)) = answer.get("challenge_id") else {
                return None;
            };
            let credential = answer.get("credential")?;
            if challenge.is_empty()
                || challenge.len() > 64
                || !challenge.bytes().all(|b| b.is_ascii_graphic())
                || !credential.is_object()
            {
                return None;
            }
            Some((challenge.clone(), credential.clone()))
        }
        Some(_) => return None,
    };
    let recovery = match fields.get("recovery_code") {
        None | Some(Value::Null) => None,
        Some(Value::String(code)) => Some(recovery_symbols(code)?),
        Some(_) => return None,
    };
    let factor = match (code, passkey, recovery) {
        (Some(code), None, None) => Factor::Totp(code),
        (None, Some((challenge, credential)), None) => Factor::Passkey(challenge, credential),
        (None, None, Some(symbols)) => Factor::Recovery(symbols),
        (None, None, None) => Factor::None,
        _ => return None,
    };
    Some(Finish {
        token,
        password: password.to_string(),
        factor,
    })
}

/// The 16 normalized symbols of a recovery code as typed, or `None` when
/// it is not one.
fn recovery_symbols(typed: &str) -> Option<String> {
    const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    if typed.len() > 40 {
        return None;
    }
    let mut symbols = String::new();
    for c in typed.chars() {
        let c = match c {
            ' ' | '-' => continue,
            'i' | 'I' | 'l' | 'L' => '1',
            'o' | 'O' => '0',
            c if c.is_ascii_lowercase() => c.to_ascii_uppercase(),
            c => c,
        };
        if !CROCKFORD.contains(c) {
            return None;
        }
        symbols.push(c);
    }
    (symbols.len() == 16).then_some(symbols)
}

/// The fields of a body that is one JSON object with no repeated keys and
/// no keys outside `allowed`; `None` otherwise.
fn object_of(body: &[u8], allowed: &[&str]) -> Option<Map<String, Value>> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    if has_repeated_top_level_key(body) || fields.keys().any(|k| !allowed.contains(&k.as_str())) {
        return None;
    }
    Some(fields)
}

/// A link token: exactly 64 lowercase hex characters.
fn link_token(value: &Value) -> Option<String> {
    match value {
        Value::String(token)
            if token.len() == 64 && token.chars().all(|c| "0123456789abcdef".contains(c)) =>
        {
            Some(token.clone())
        }
        _ => None,
    }
}

/// What every accepted address has: at most 254 bytes, something before
/// its last `@`, and a dotted domain after it.
fn could_be_address(address: &str) -> bool {
    address.len() <= EMAIL_MAX
        && address
            .rsplit_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
}

/// An address no reasonable reading refuses: a local part of letters,
/// digits and `._+-` that neither starts nor ends with a dot nor repeats
/// one, then two or more labels of letters and digits (inner hyphens
/// allowed), the last all letters.
fn is_plain_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    let local_ok = (1..=64).contains(&local.len())
        && local
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._+-".contains(&b))
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..");
    let labels: Vec<&str> = domain.split('.').collect();
    let labels_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_lowercase()));
    local_ok && labels_ok && address.len() <= EMAIL_MAX
}

/// Keys of a JSON object, in the order written.
struct Keys;

impl<'de> Visitor<'de> for Keys {
    type Value = Vec<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<String>, A::Error> {
        let mut keys = Vec::new();
        while let Some((key, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? {
            keys.push(key);
        }
        Ok(keys)
    }
}

/// True when `keys` names one key twice.
fn repeats(mut keys: Vec<String>) -> bool {
    let written = keys.len();
    keys.sort();
    keys.dedup();
    keys.len() != written
}

/// True when the top-level JSON object in `data` writes a key twice.
fn has_repeated_top_level_key(data: &[u8]) -> bool {
    let mut de = serde_json::Deserializer::from_slice(data);
    de.deserialize_map(Keys).is_ok_and(repeats)
}

/// True when the `passkey` object of the top-level object in `data` writes
/// a key twice.
fn passkey_has_repeated_key(data: &[u8]) -> bool {
    /// The keys of the top-level object's `passkey` value, when it is an
    /// object.
    struct Passkey;

    /// The keys of one value: `Some` for an object.
    struct MaybeKeys(Option<Vec<String>>);

    /// Keys of an object, or nothing for another value.
    struct Any;

    impl<'de> Visitor<'de> for Any {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("any JSON value")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            Keys.visit_map(map).map(Some)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    impl<'de> serde::Deserialize<'de> for MaybeKeys {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Any).map(Self)
        }
    }

    impl<'de> Visitor<'de> for Passkey {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut found = None;
            while let Some(key) = map.next_key::<String>()? {
                if key == "passkey" {
                    found = map.next_value::<MaybeKeys>()?.0;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(found)
        }
    }

    let mut de = serde_json::Deserializer::from_slice(data);
    de.deserialize_map(Passkey)
        .ok()
        .flatten()
        .is_some_and(repeats)
}
