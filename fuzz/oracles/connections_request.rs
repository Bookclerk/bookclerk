//! Shared oracle for the `connections_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the bodies of `POST /api/config/join/connections` and
//! `POST /api/config/join/connections/disconnect`, which a branch's signed-in
//! Owner sends to add their store connections to a library or to disconnect
//! one ([`parse_add_connections_request`], [`parse_disconnect_request`]).
//! Each input is fed to both parsers. The invariant, checked against this
//! file's own reading of the bytes as an untyped JSON value rather than the
//! typed bodies:
//!
//! - An add body is accepted exactly when it is one JSON object with no
//!   repeated keys anywhere, no keys besides `cluster_id` (a string) and
//!   `accounts` (both present), and `accounts` is an array of 1 to 64
//!   objects, each with exactly a string `plugin_key` and a string
//!   `account_id`. It yields the trimmed `cluster_id` and the accounts as
//!   sent, in order.
//! - A disconnect body is accepted exactly when it is one JSON object with no
//!   repeated keys, a string `plugin_key`, a string `account_id`, and
//!   `everywhere` absent (read as `false`) or a boolean, and no other key. It
//!   yields those values as sent.
//!
//! A refused body is `Invalid`, never another error.

use bookclerk_library::control_plane::{
    parse_add_connections_request, parse_disconnect_request, AccountRef,
};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking; a body naming 64 accounts is a few KiB.
pub const MAX_INPUT_BYTES: usize = 16 * 1024;

/// Most accounts an add body names, written out rather than read from the
/// parser.
const MAX_ACCOUNTS: usize = 64;

/// Runs the oracle on one input, against both bodies.
///
/// # Arguments
///
/// * `data` - The request body bytes.
///
/// # Returns
///
/// True when either parser accepted the body.
///
/// # Panics
///
/// When a parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let object = match serde_json::from_slice::<Value>(data) {
        Ok(Value::Object(fields)) if !has_repeated_key(data) => Some(fields),
        _ => None,
    };
    let add = check_add(data, object.as_ref());
    let disconnect = check_disconnect(data, object.as_ref());
    add || disconnect
}

/// Checks the add parser against the add rules; true when accepted.
fn check_add(data: &[u8], object: Option<&Map<String, Value>>) -> bool {
    let got = parse_add_connections_request(data);
    match object.and_then(expected_add) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "add {:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Some((cluster_id, accounts)) => {
            let request = got.unwrap_or_else(|err| {
                panic!("add {:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(request.cluster_id, cluster_id);
            assert_eq!(request.accounts, accounts);
            true
        }
    }
}

/// Checks the disconnect parser against the disconnect rules; true when
/// accepted.
fn check_disconnect(data: &[u8], object: Option<&Map<String, Value>>) -> bool {
    let got = parse_disconnect_request(data);
    match object.and_then(expected_disconnect) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "disconnect {:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Some((account, everywhere)) => {
            let request = got.unwrap_or_else(|err| {
                panic!(
                    "disconnect {:?} refused: {err:?}",
                    String::from_utf8_lossy(data)
                )
            });
            assert_eq!(request.account, account);
            assert_eq!(request.everywhere, everywhere);
            true
        }
    }
}

/// The trimmed library id and accounts an add body should yield, or `None`
/// when it should be refused.
fn expected_add(fields: &Map<String, Value>) -> Option<(String, Vec<AccountRef>)> {
    if fields.len() != 2 {
        return None;
    }
    let Some(Value::String(cluster_id)) = fields.get("cluster_id") else {
        return None;
    };
    let Some(Value::Array(items)) = fields.get("accounts") else {
        return None;
    };
    if items.is_empty() || items.len() > MAX_ACCOUNTS {
        return None;
    }
    let accounts = items
        .iter()
        .map(|item| match item {
            Value::Object(account) if account.len() == 2 => account_ref(account),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((cluster_id.trim().to_string(), accounts))
}

/// The account and `everywhere` a disconnect body should yield, or `None`
/// when it should be refused.
fn expected_disconnect(fields: &Map<String, Value>) -> Option<(AccountRef, bool)> {
    let everywhere = match fields.get("everywhere") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return None,
    };
    let known = 2 + usize::from(fields.contains_key("everywhere"));
    if fields.len() != known {
        return None;
    }
    Some((account_ref(fields)?, everywhere))
}

/// Reads string `plugin_key` and `account_id` from an object; the caller
/// has checked there are no other keys.
fn account_ref(fields: &Map<String, Value>) -> Option<AccountRef> {
    let (Some(Value::String(plugin_key)), Some(Value::String(account_id))) =
        (fields.get("plugin_key"), fields.get("account_id"))
    else {
        return None;
    };
    Some(AccountRef {
        plugin_key: plugin_key.clone(),
        account_id: account_id.clone(),
    })
}

/// True when any JSON object in `data` writes a key twice. An untyped
/// object keeps only the last of a repeated key, while the bodies refuse
/// repeats, so the keys are counted as written. `data` already parsed.
fn has_repeated_key(data: &[u8]) -> bool {
    /// Walks any JSON value; its result is whether a repeat was seen.
    struct Walk;

    impl<'de> serde::de::DeserializeSeed<'de> for Walk {
        type Value = bool;

        fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<bool, D::Error> {
            de.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for Walk {
        type Value = bool;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("any JSON value")
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
            while let Some(inner) = seq.next_element_seed(Walk)? {
                repeated |= inner;
            }
            Ok(repeated)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let mut keys = std::collections::BTreeSet::new();
            let mut repeated = false;
            while let Some(key) = map.next_key::<String>()? {
                repeated |= !keys.insert(key);
                repeated |= map.next_value_seed(Walk)?;
            }
            Ok(repeated)
        }
    }

    let mut de = serde_json::Deserializer::from_slice(data);
    de.deserialize_any(Walk).unwrap_or(false)
}
