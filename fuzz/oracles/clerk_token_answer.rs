//! Shared oracle for the `clerk_token_answer` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the library's answer to `POST /api/clerk/v1/token`,
//! read on the clerk (`bookclerk-node-proto::parse_clerk_token_answer`):
//! another machine's bytes that become a sealed library card and a stored
//! link.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value: the answer parses exactly when it is at most
//! 8 KiB and one object, none of the fields below is written twice,
//! `token` is 43
//! base64url characters, `library_id` is 1 to 128 bytes of ASCII letters,
//! digits, `-`, `_`, `.` and `:`, `user` is an object whose `id` is a positive integer and whose `display_name`, when present
//! and not `null`, is a string of at most 128 bytes with no control
//! characters, and `idle_secs` is an integer from 1 to a year (31,622,400
//! seconds). Other keys are ignored. Every field of the parsed answer is
//! the value sent.

use bookclerk_node_proto::parse_clerk_token_answer;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: one byte past the answer limit.
pub const MAX_INPUT_BYTES: usize = 8 * 1024 + 1;

/// The answer limit, written out.
const MAX_ANSWER_BYTES: usize = 8 * 1024;

/// A year of seconds, written out.
const YEAR_SECS: i64 = 366 * 24 * 60 * 60;

/// Checks one input against the invariant.
///
/// # Arguments
///
/// * `data` - The answer body.
///
/// # Returns
///
/// True when the answer parsed.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
#[must_use]
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let parsed = parse_clerk_token_answer(data);
    let expected = expected(data);
    let text = String::from_utf8_lossy(data);
    match (&parsed, &expected) {
        (None, None) => false,
        (Some(answer), Some((token, library_id, id, name, idle))) => {
            assert_eq!(&answer.token, token, "{text:?}");
            assert_eq!(&answer.library_id, library_id, "{text:?}");
            assert_eq!(answer.user.id, *id, "{text:?}");
            assert_eq!(&answer.user.display_name, name, "{text:?}");
            assert_eq!(answer.idle_secs, *idle, "{text:?}");
            true
        }
        (Some(_), None) => panic!("a bad answer parsed: {text:?}"),
        (None, Some(_)) => panic!("a good answer was refused: {text:?}"),
    }
}

/// The fields of a good answer: token, library id, user id, display name
/// and idle seconds.
type Fields = (String, String, i64, Option<String>, i64);

/// What `data` should parse as, worked out here.
fn expected(data: &[u8]) -> Option<Fields> {
    if data.len() > MAX_ANSWER_BYTES {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(data) else {
        return None;
    };
    if repeats_key(data) {
        return None;
    }
    let token = fields.get("token")?.as_str()?;
    let token_ok = token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let library_id = fields.get("library_id")?.as_str()?;
    let library_ok = (1..=128).contains(&library_id.len())
        && library_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    let Value::Object(user) = fields.get("user")? else {
        return None;
    };
    let id = user.get("id")?.as_i64().filter(|id| *id > 0)?;
    let name = match user.get("display_name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(name.clone()),
        Some(_) => return None,
    };
    let name_ok = name
        .as_deref()
        .is_none_or(|name| name.len() <= 128 && !name.chars().any(char::is_control));
    let idle = fields
        .get("idle_secs")?
        .as_i64()
        .filter(|idle| (1..=YEAR_SECS).contains(idle))?;
    (token_ok && library_ok && name_ok)
        .then(|| (token.to_owned(), library_id.to_owned(), id, name, idle))
}

/// The keys written at the top level, and those written in each `user`
/// value that is an object.
#[derive(Default)]
struct Written {
    /// Top-level keys, in order.
    top: Vec<String>,
    /// For each `user` key, the keys of its object (empty when not one).
    users: Vec<Vec<String>>,
}

/// Collects [`Written`].
struct Top;

impl<'de> Visitor<'de> for Top {
    type Value = Written;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Written, A::Error> {
        let mut written = Written::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == "user" {
                written.users.push(map.next_value_seed(UserKeys)?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
            written.top.push(key);
        }
        Ok(written)
    }
}

/// Reads one value, answering its keys when it is an object.
struct UserKeys;

impl<'de> serde::de::DeserializeSeed<'de> for UserKeys {
    type Value = Vec<String>;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Vec<String>, D::Error> {
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UserKeys {
    type Value = Vec<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<String>, A::Error> {
        let mut keys = Vec::new();
        while let Some((key, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? {
            keys.push(key);
        }
        Ok(keys)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Vec::new())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Vec<String>, E> {
        Ok(Vec::new())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Vec<String>, E> {
        Ok(Vec::new())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Vec<String>, E> {
        Ok(Vec::new())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Vec<String>, E> {
        Ok(Vec::new())
    }

    fn visit_str<E>(self, _: &str) -> Result<Vec<String>, E> {
        Ok(Vec::new())
    }

    fn visit_unit<E>(self) -> Result<Vec<String>, E> {
        Ok(Vec::new())
    }
}

/// How many times `keys` writes `name`.
fn count(keys: &[String], name: &str) -> usize {
    keys.iter().filter(|key| key.as_str() == name).count()
}

/// True when a field the answer reads is written twice: a top-level
/// `token`, `library_id`, `user` or `idle_secs`, or `id` or
/// `display_name` inside `user`. Other keys may repeat.
fn repeats_key(data: &[u8]) -> bool {
    let mut de = serde_json::Deserializer::from_slice(data);
    let Ok(written) = de.deserialize_map(Top) else {
        return false;
    };
    ["token", "library_id", "user", "idle_secs"]
        .iter()
        .any(|name| count(&written.top, name) > 1)
        || written
            .users
            .iter()
            .any(|keys| count(keys, "id") > 1 || count(keys, "display_name") > 1)
}
