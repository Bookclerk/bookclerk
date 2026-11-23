//! Shared oracle for the `member_controls_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the member controls an Owner or Administrator sends:
//! the body of `PATCH /api/users/default-controls`
//! ([`parse_default_controls_patch`]) and of
//! `PATCH /api/users/{id}/controls` ([`parse_user_controls_patch`]). The
//! first input byte picks the body (`d` for the default, `u` for a
//! patron's overrides); the rest is the body.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value rather than the typed body:
//!
//! - Each body is one JSON object with no repeated keys and exactly the
//!   key `changes`, an object with no repeated keys whose keys are among
//!   the six control names (`link_accounts`, `download`, `edit_metadata`,
//!   `upload`, `request_books`, `see_prices`).
//! - In the default's changes each value is a boolean (the new default) or
//!   `null`; in a patron's, each is the string `on`, `off` or `default`,
//!   or `null`. A control left out or `null` is not changed.
//!
//! An accepted body yields exactly those changes; a refused one is
//! `Invalid`, never another error.

use std::collections::BTreeSet;

use bookclerk_library::control_plane::{
    parse_default_controls_patch, parse_user_controls_patch, ControlChanges, ControlOverrides,
    MemberControl,
};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking: every control written twice over with
/// room for whitespace.
pub const MAX_INPUT_BYTES: usize = 4 * 1024;

/// The control names the routes document, in their order.
const DOCUMENTED_NAMES: [&str; 6] = [
    "link_accounts",
    "download",
    "edit_metadata",
    "upload",
    "request_books",
    "see_prices",
];

/// A change per documented control, in [`DOCUMENTED_NAMES`] order: `None`
/// for a control the body leaves out or sets to `null`, `Some(Some(v))` to
/// set it to `v`, and `Some(None)` to make a patron follow the default.
type Values = [Option<Option<bool>>; 6];

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A selector byte (`d` or `u`) followed by the body.
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
    let Some((&selector, body)) = data.split_first() else {
        return false;
    };
    let (got, expected) = match selector {
        b'd' => (
            parse_default_controls_patch(body).map(|changes| values_of_default(&changes)),
            expected_values(body, read_default_value),
        ),
        b'u' => (
            parse_user_controls_patch(body).map(|changes| values_of_changes(&changes)),
            expected_values(body, read_choice),
        ),
        _ => return false,
    };
    match expected {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Some(values) => {
            let got = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(got, values, "{:?}", String::from_utf8_lossy(data));
            true
        }
    }
}

/// The parser's default changes as oracle values.
fn values_of_default(changes: &ControlOverrides) -> Values {
    MemberControl::ALL.map(|control| changes.get(control).map(Some))
}

/// The parser's patron changes as oracle values.
fn values_of_changes(changes: &ControlChanges) -> Values {
    MemberControl::ALL.map(|control| changes.get(control).map(|choice| choice.value()))
}

/// The expected changes for a body, or `None` when it is refused;
/// `read_value` reads one control's value (`None` to refuse it, `Some(None)`
/// for no change).
fn expected_values(
    body: &[u8],
    read_value: fn(&Value) -> Option<Option<Option<bool>>>,
) -> Option<Values> {
    if has_repeated_key(body) {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    if fields.len() != 1 {
        return None;
    }
    let Some(Value::Object(changes)) = fields.get("changes") else {
        return None;
    };
    read_values(changes, read_value)
}

/// One value of the default's changes: a boolean, or `null` for no change.
fn read_default_value(value: &Value) -> Option<Option<Option<bool>>> {
    match value {
        Value::Bool(flag) => Some(Some(Some(*flag))),
        Value::Null => Some(None),
        _ => None,
    }
}

/// One value of a patron's changes: `on`, `off`, `default`, or `null` for
/// no change.
fn read_choice(value: &Value) -> Option<Option<Option<bool>>> {
    match value {
        Value::String(choice) => match choice.as_str() {
            "on" => Some(Some(Some(true))),
            "off" => Some(Some(Some(false))),
            "default" => Some(Some(None)),
            _ => None,
        },
        Value::Null => Some(None),
        _ => None,
    }
}

/// Reads the documented controls out of one object, refusing an unknown
/// key or a value `read_value` refuses.
fn read_values(
    fields: &Map<String, Value>,
    read_value: fn(&Value) -> Option<Option<Option<bool>>>,
) -> Option<Values> {
    let mut values: Values = [None; 6];
    for (key, value) in fields {
        let index = DOCUMENTED_NAMES.iter().position(|name| name == key)?;
        values[index] = read_value(value)?;
    }
    Some(values)
}

/// True when any JSON object in `data`, at any depth, writes a key twice.
/// An untyped value keeps only the last of a repeated key, while the typed
/// bodies refuse repeats, so the keys are counted as written.
fn has_repeated_key(data: &[u8]) -> bool {
    /// Walks a value, noting whether some object repeats a key.
    struct Walk;

    impl<'de> Visitor<'de> for Walk {
        type Value = bool;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON value")
        }

        fn visit_bool<E>(self, _: bool) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_i64<E>(self, _: i64) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_u64<E>(self, _: u64) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_f64<E>(self, _: f64) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_str<E>(self, _: &str) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_unit<E>(self) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
            let mut repeated = false;
            while let Some(inner) = seq.next_element::<Repeated>()? {
                repeated |= inner.0;
            }
            Ok(repeated)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let mut keys = BTreeSet::new();
            let mut repeated = false;
            while let Some(key) = map.next_key::<String>()? {
                repeated |= !keys.insert(key);
                repeated |= map.next_value::<Repeated>()?.0;
            }
            Ok(repeated)
        }
    }

    /// Whether a nested value repeats a key.
    struct Repeated(bool);

    impl<'de> serde::Deserialize<'de> for Repeated {
        fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_any(Walk).map(Repeated)
        }
    }

    let mut de = serde_json::Deserializer::from_slice(data);
    de.deserialize_any(Walk).unwrap_or(false)
}
