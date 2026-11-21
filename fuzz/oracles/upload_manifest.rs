//! Shared oracle for the `upload_manifest` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is what a patron sends to start and fill an upload: the
//! `POST /api/uploads` manifest ([`parse_upload_manifest`]) and the
//! `Upload-Offset` header of each `PATCH` ([`parse_upload_offset`]). The
//! first input byte picks the input (`m` for a manifest, `o` for an
//! offset); the rest is the input.
//!
//! The invariant, checked against this file's own reading of the bytes:
//!
//! - A manifest is accepted exactly when it is JSON with no repeated keys
//!   holding `files` (an object with only that key, or the one-element
//!   array serde also reads as the struct), at most 10,000 files, each an
//!   object with `path` (string), `size` (unsigned integer) and optional
//!   `sha256` (`null` or 64 hex digits) and nothing else, or the
//!   two-or-three-element array form, and every path passes the archive
//!   path policy. The specs keep order, sizes, digests (folded to lower
//!   case) and admitted paths, and no admitted path is absolute, climbs
//!   with `..`, holds an empty or `.` component, a backslash or a NUL.
//! - An offset is accepted exactly when it is 1 to 19 ASCII digits whose
//!   value fits `i64`, and yields that value.

use std::collections::BTreeSet;

use bookclerk_archive::EntryPath;
use bookclerk_library::control_plane::{
    parse_upload_manifest, parse_upload_offset, MemberSpec, MAX_BATCH_FILES,
};
use serde::de::{Deserializer as _, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// One file as this oracle reads it: raw path, size and digest.
type File = (String, u64, Option<String>);

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A selector byte (`m` or `o`) followed by the input.
///
/// # Returns
///
/// True when the parser accepted the input.
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
    match selector {
        b'm' => check_manifest(body),
        b'o' => check_offset(body),
        _ => false,
    }
}

/// Checks the manifest parser against [`expected_manifest`].
fn check_manifest(body: &[u8]) -> bool {
    let got = parse_upload_manifest(body);
    match expected_manifest(body) {
        None => {
            assert!(
                got.is_err(),
                "{:?} accepted: {got:?}",
                String::from_utf8_lossy(body)
            );
            false
        }
        Some(files) => {
            let specs = got
                .unwrap_or_else(|err| panic!("{:?} refused: {err}", String::from_utf8_lossy(body)));
            assert_eq!(specs.len(), files.len());
            for (spec, (path, size, sha256)) in specs.iter().zip(&files) {
                check_spec(spec, path, *size, sha256.as_deref());
            }
            true
        }
    }
}

/// Checks one accepted spec against the file it came from.
fn check_spec(spec: &MemberSpec, path: &str, size: u64, sha256: Option<&str>) {
    assert_eq!(spec.size, size);
    assert!(spec.intake.is_none());
    assert_eq!(
        spec.sha256
            .as_ref()
            .map(|digest| digest.as_str().to_string()),
        sha256.map(str::to_ascii_lowercase)
    );
    assert_eq!(Some(&spec.path), EntryPath::admit(path).ok().as_ref());
    let admitted = spec.path.as_str();
    assert!(
        !admitted.is_empty() && !admitted.starts_with('/'),
        "{admitted:?}"
    );
    assert!(
        !admitted.contains('\\') && !admitted.contains('\0'),
        "{admitted:?}"
    );
    assert!(
        admitted
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "{admitted:?}"
    );
}

/// The files of an acceptable manifest, or `None` when it is refused.
fn expected_manifest(body: &[u8]) -> Option<Vec<File>> {
    if has_repeated_key(body) {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let files = match value {
        Value::Object(fields) if fields.len() == 1 => fields.get("files")?.clone(),
        Value::Array(mut items) if items.len() == 1 => items.pop()?,
        _ => return None,
    };
    let Value::Array(files) = files else {
        return None;
    };
    // The parser reads every file before it counts them.
    let files = files.iter().map(file_of).collect::<Option<Vec<_>>>()?;
    if files.len() > MAX_BATCH_FILES {
        return None;
    }
    for (path, _, sha256) in &files {
        EntryPath::admit(path).ok()?;
        if let Some(digest) = sha256 {
            if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
        }
    }
    Some(files)
}

/// One file in object or array form, or `None` when its shape is wrong.
fn file_of(value: &Value) -> Option<File> {
    let digest = |value: Option<&Value>| match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(raw)) => Some(Some(raw.clone())),
        Some(_) => None,
    };
    match value {
        Value::Object(fields) => {
            if fields
                .keys()
                .any(|key| !matches!(key.as_str(), "path" | "size" | "sha256"))
            {
                return None;
            }
            let path = fields.get("path")?.as_str()?.to_string();
            let size = fields.get("size")?.as_u64()?;
            Some((path, size, digest(fields.get("sha256"))?))
        }
        Value::Array(items) if (2..=3).contains(&items.len()) => {
            let path = items.first()?.as_str()?.to_string();
            let size = items.get(1)?.as_u64()?;
            Some((path, size, digest(items.get(2))?))
        }
        _ => None,
    }
}

/// Checks the offset parser against its own reading of the digits.
fn check_offset(body: &[u8]) -> bool {
    let Ok(raw) = std::str::from_utf8(body) else {
        return false;
    };
    let expected = (!raw.is_empty() && raw.len() <= 19 && raw.bytes().all(|b| b.is_ascii_digit()))
        .then(|| raw.parse::<u128>().ok())
        .flatten()
        .filter(|value| *value <= u128::from(i64::MAX.unsigned_abs()))
        .and_then(|value| u64::try_from(value).ok());
    let got = parse_upload_offset(raw);
    assert_eq!(got, expected, "{raw:?}");
    got.is_some()
}

/// True when some JSON object in `data` repeats a key (`serde_json::Value`
/// keeps only the last, while the typed parser refuses the repeat).
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
