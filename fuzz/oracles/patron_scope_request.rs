//! Shared oracle for the `patron_scope_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is a patron scope an Owner or Administrator sends: the body
//! of `PUT /api/users/default-scope` ([`parse_default_scope_request`]) and of
//! `PUT /api/users/{id}/scope` ([`parse_user_scope_request`]). The first
//! input byte picks the body (`d` for the default scope, `u` for a user's);
//! the rest is the body.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value rather than the typed body:
//!
//! - A scope is one JSON object with no repeated keys and at most the keys
//!   `rules`, an array (missing means no rules) of at most 16 rules, and
//!   `explicit`, the string `allow` or `hide` (missing means `allow`). Each
//!   rule is an object with no repeated keys and exactly the keys `field`
//!   (`genre`, `series`, `author`, `narrator`, `tag` or `collection`), `value` (a string) and
//!   `effect` (`include` or `exclude`). Each value, trimmed, is 1 to 256
//!   bytes with no NUL, no two rules share field, effect and trimmed
//!   lowercase value, and the rules with trimmed values (plus
//!   `"explicit":"hide"` when hiding) come to at most 1,024 bytes as
//!   compact JSON.
//! - Both bodies carry `expected_version`, a string of 32 lowercase hex
//!   digits.
//! - The default body is one object with no repeated keys and exactly the
//!   keys `scope`, such a scope, and `expected_version`.
//! - A user's body is one object with no repeated keys, the key
//!   `expected_version` and at most the key `scope` beside it, which is
//!   `null` (or missing) or such a scope.
//!
//! An accepted body yields the rules in order with trimmed values and the
//! explicit choice (or no scope), and the version as sent; a refused one is
//! `Invalid`, never another error.

use std::collections::BTreeSet;

use bookclerk_library::control_plane::{
    parse_default_scope_request, parse_user_scope_request, ExplicitContent, PatronScope,
    RuleEffect, ScopeField,
};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking: eight times the largest scope's JSON,
/// room for whitespace and escapes that the limit counts compactly.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

/// Most rules the routes document.
const DOCUMENTED_MAX_RULES: usize = 16;

/// Longest trimmed rule value the routes document, in bytes.
const DOCUMENTED_MAX_VALUE_BYTES: usize = 256;

/// Largest scope as compact JSON, in bytes, as the API documents it.
const DOCUMENTED_MAX_SCOPE_JSON_BYTES: usize = 1024;

/// One rule as the oracle reads it: field, trimmed value, effect.
type Rule = (ScopeField, String, RuleEffect);

/// A scope as the oracle reads it: its rules and the explicit choice.
type Scope = (Vec<Rule>, ExplicitContent);

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with this scope (or no scope at all for a user's body
    /// that returns them to the default) and this expected version.
    Accepted(Option<Scope>, String),
}

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
            parse_default_scope_request(body)
                .map(|body| (Some(body.scope), body.expected_version.as_str().to_owned())),
            expected_default(body),
        ),
        b'u' => (
            parse_user_scope_request(body)
                .map(|body| (body.scope, body.expected_version.as_str().to_owned())),
            expected_user(body),
        ),
        _ => return false,
    };
    match expected {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(expected, version) => {
            let (scope, got_version) = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            let scope: Option<Scope> = scope.as_ref().map(scope_of);
            assert_eq!(scope, expected, "{:?}", String::from_utf8_lossy(data));
            assert_eq!(got_version, version, "{:?}", String::from_utf8_lossy(data));
            true
        }
    }
}

/// The parser's scope as the oracle reads one.
fn scope_of(scope: &PatronScope) -> Scope {
    (
        scope
            .rules
            .iter()
            .map(|rule| (rule.field, rule.value.clone(), rule.effect))
            .collect(),
        scope.explicit,
    )
}

/// The expected outcome for a default-scope body.
fn expected_default(body: &[u8]) -> Expected {
    if has_repeated_key(body) {
        return Expected::Refused;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return Expected::Refused;
    };
    if fields
        .keys()
        .any(|key| key != "scope" && key != "expected_version")
    {
        return Expected::Refused;
    }
    let Some(version) = expected_version(&fields) else {
        return Expected::Refused;
    };
    match fields.get("scope") {
        Some(Value::Object(scope)) => match scope_rules(scope) {
            Some(scope) => Expected::Accepted(Some(scope), version),
            None => Expected::Refused,
        },
        _ => Expected::Refused,
    }
}

/// The expected outcome for a user's scope body.
fn expected_user(body: &[u8]) -> Expected {
    if has_repeated_key(body) {
        return Expected::Refused;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return Expected::Refused;
    };
    if fields
        .keys()
        .any(|key| key != "scope" && key != "expected_version")
    {
        return Expected::Refused;
    }
    let Some(version) = expected_version(&fields) else {
        return Expected::Refused;
    };
    match fields.get("scope") {
        None | Some(Value::Null) => Expected::Accepted(None, version),
        Some(Value::Object(scope)) => match scope_rules(scope) {
            Some(scope) => Expected::Accepted(Some(scope), version),
            None => Expected::Refused,
        },
        Some(_) => Expected::Refused,
    }
}

/// The body's `expected_version` when it is the documented 32 lowercase
/// hex digits, or `None`.
fn expected_version(fields: &Map<String, Value>) -> Option<String> {
    match fields.get("expected_version")? {
        Value::String(version)
            if version.len() == 32
                && version
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) =>
        {
            Some(version.clone())
        }
        _ => None,
    }
}

/// One scope object as the oracle reads it, or `None` when the documented
/// rules refuse it.
fn scope_rules(fields: &Map<String, Value>) -> Option<Scope> {
    if fields.keys().any(|key| key != "rules" && key != "explicit") {
        return None;
    }
    let explicit = match fields.get("explicit") {
        None => ExplicitContent::Allow,
        Some(Value::String(name)) => match name.as_str() {
            "allow" => ExplicitContent::Allow,
            "hide" => ExplicitContent::Hide,
            _ => return None,
        },
        Some(_) => return None,
    };
    let empty = Vec::new();
    let items = match fields.get("rules") {
        None => &empty,
        Some(Value::Array(items)) => items,
        Some(_) => return None,
    };
    if items.len() > DOCUMENTED_MAX_RULES {
        return None;
    }
    let mut seen = BTreeSet::new();
    let mut rules = Vec::with_capacity(items.len());
    let mut written = Vec::with_capacity(items.len());
    for item in items {
        let Value::Object(rule) = item else {
            return None;
        };
        if rule.len() != 3 {
            return None;
        }
        let field = match rule.get("field")? {
            Value::String(name) => match name.as_str() {
                "genre" => ScopeField::Genre,
                "series" => ScopeField::Series,
                "author" => ScopeField::Author,
                "narrator" => ScopeField::Narrator,
                "tag" => ScopeField::Tag,
                "collection" => ScopeField::Collection,
                _ => return None,
            },
            _ => return None,
        };
        let effect = match rule.get("effect")? {
            Value::String(name) => match name.as_str() {
                "include" => RuleEffect::Include,
                "exclude" => RuleEffect::Exclude,
                _ => return None,
            },
            _ => return None,
        };
        let Value::String(value) = rule.get("value")? else {
            return None;
        };
        let value = value.trim();
        if value.is_empty() || value.len() > DOCUMENTED_MAX_VALUE_BYTES || value.contains('\0') {
            return None;
        }
        if !seen.insert((field, value.to_lowercase(), effect)) {
            return None;
        }
        written.push(serde_json::json!({
            "field": rule.get("field")?,
            "value": value,
            "effect": rule.get("effect")?,
        }));
        rules.push((field, value.to_string(), effect));
    }
    let compact = if explicit == ExplicitContent::Hide {
        serde_json::json!({ "rules": written, "explicit": "hide" })
    } else {
        serde_json::json!({ "rules": written })
    };
    let size = serde_json::to_vec(&compact).ok()?.len();
    if size > DOCUMENTED_MAX_SCOPE_JSON_BYTES {
        return None;
    }
    Some((rules, explicit))
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
