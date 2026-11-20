//! Settings of a drop folder: a storage plugin instance with the intake
//! role (see `docs/server/drop-folders.md`).
//!
//! An intake instance is never an acquire destination. The host lists it,
//! waits until each item in it has stopped changing for
//! [`IntakeSettings::settle_seconds`], hands the item to the ingest job and
//! then moves, deletes or leaves the source as
//! [`IntakeSettings::after_ingest`] says.
//!
//! The settings are host data, not a plugin setting: they live in the
//! instance's `core.intake` document, and that document's presence is what
//! gives the instance the intake role. Operators write them over the web
//! API and the CLI, so [`parse_intake_settings`] takes only the documented
//! JSON shape: one object, no unknown or repeated keys, every bound held.
//! The `intake_settings` fuzz target checks it against an independent
//! reading.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default quiet time before an item is ingested, in seconds.
pub const DEFAULT_SETTLE_SECONDS: u32 = 60;

/// Shortest quiet time an operator may set, in seconds.
pub const MIN_SETTLE_SECONDS: u32 = 5;

/// Longest quiet time an operator may set, in seconds (one day).
pub const MAX_SETTLE_SECONDS: u32 = 86_400;

/// Default time between two listings of a drop folder, in seconds.
pub const DEFAULT_SCAN_INTERVAL_SECONDS: u32 = 300;

/// Shortest time between two listings an operator may set, in seconds.
pub const MIN_SCAN_INTERVAL_SECONDS: u32 = 30;

/// Longest time between two listings an operator may set, in seconds.
pub const MAX_SCAN_INTERVAL_SECONDS: u32 = 86_400;

/// Most extra ignore patterns one drop folder may hold.
pub const MAX_IGNORE_PATTERNS: usize = 64;

/// Longest ignore pattern, in UTF-8 bytes.
pub const MAX_IGNORE_PATTERN_BYTES: usize = 128;

/// Longest time `_imported` folders may be kept before pruning, in days
/// (about ten years).
pub const MAX_PRUNE_IMPORTED_DAYS: u16 = 3_650;

/// Largest settings body [`parse_intake_settings`] reads, in bytes.
pub const MAX_INTAKE_SETTINGS_BYTES: usize = 16 * 1024;

/// What happens to a drop folder item once it has been ingested.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AfterIngest {
    /// Move the item to `_imported/<date>/` inside the drop folder (a failed
    /// item goes to `_failed/`).
    #[default]
    Move,
    /// Delete the item once every destination copy is stored (a failed item
    /// goes to `_failed/`).
    Delete,
    /// Leave the item where it is and remember its signature so it is not
    /// ingested twice.
    Leave,
}

impl AfterIngest {
    /// Every value, in the order the names are documented.
    pub const ALL: [Self; 3] = [Self::Move, Self::Delete, Self::Leave];

    /// The value's name on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Delete => "delete",
            Self::Leave => "leave",
        }
    }

    /// Reads a value by its wire name ([`Self::as_str`]), exactly.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|value| value.as_str() == name)
    }

    /// True when the drop folder plugin must be able to write (move or
    /// delete); `leave` only reads.
    #[must_use]
    pub fn writes(self) -> bool {
        match self {
            Self::Move | Self::Delete => true,
            Self::Leave => false,
        }
    }
}

/// Who holds the books a drop folder ingests.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum IntakeAttribution {
    /// Every item is held by [`IntakeSettings::patron`].
    #[default]
    Owner,
    /// Each top-level folder is named after a patron's username and holds
    /// that patron's items; an item one level deeper is one book.
    Subfolder,
}

impl IntakeAttribution {
    /// Every value, in the order the names are documented.
    pub const ALL: [Self; 2] = [Self::Owner, Self::Subfolder];

    /// The value's name on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Subfolder => "subfolder",
        }
    }

    /// Reads a value by its wire name ([`Self::as_str`]), exactly.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|value| value.as_str() == name)
    }

    /// How many leading path segments name one item: the item itself, plus
    /// the patron folder in `subfolder` mode.
    #[must_use]
    pub fn item_depth(self) -> usize {
        match self {
            Self::Owner => 1,
            Self::Subfolder => 2,
        }
    }
}

/// The settings of one drop folder (`core.intake`).
///
/// Every field has a default, so `{}` is a valid body only when
/// `attribute_to` is `subfolder`; `owner` needs a `patron`.
// Read by a strict hand-written reader ([`parse_intake_settings`]); the
// serde attributes here shape the schema and the stored document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IntakeSettings {
    /// Seconds an item's files must stay unchanged before it is ingested,
    /// 5 to 86,400 (default 60).
    #[schema(minimum = 5, maximum = 86400, example = 60)]
    pub settle_seconds: u32,
    /// Seconds between two full listings of the folder, 30 to 86,400
    /// (default 300). A change feed starts a listing sooner.
    #[schema(minimum = 30, maximum = 86400, example = 300)]
    pub scan_interval_seconds: u32,
    /// What happens to an item once it is ingested (default `move`).
    pub after_ingest: AfterIngest,
    /// Who holds the ingested books (default `owner`).
    pub attribute_to: IntakeAttribution,
    /// The user id that holds every book in `owner` mode; required there
    /// and refused in `subfolder` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, example = 2)]
    pub patron: Option<i64>,
    /// Extra file name patterns to ignore, at most 64, each 1 to 128 bytes
    /// with no `/`: `*` matches any run of characters and `?` one
    /// character, compared without ASCII case. The built-in list always
    /// applies too ([`BUILTIN_IGNORE_PATTERNS`]).
    #[schema(max_items = 64, example = json!(["*.nzb"]))]
    pub ignore: Vec<String>,
    /// With `after_ingest = move`, delete `_imported` day folders older than
    /// this many days, 1 to 3,650; absent keeps them.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 3650, example = 30)]
    pub prune_imported_days: Option<u16>,
}

/// File name patterns every drop folder ignores: partial downloads,
/// editor and sync-tool temporaries, and desktop metadata files. A name
/// starting with `.` is always ignored as well.
pub const BUILTIN_IGNORE_PATTERNS: &[&str] = &[
    "*.part",
    "*.partial",
    "*.crdownload",
    "*.download",
    "*.tmp",
    "*.!qb",
    "~*",
    "desktop.ini",
    "thumbs.db",
];

/// Folder the host moves ingested items into, at the drop folder's root.
pub const IMPORTED_FOLDER: &str = "_imported";

/// Folder the host moves items that failed to ingest into, at the drop
/// folder's root.
pub const FAILED_FOLDER: &str = "_failed";

/// Why [`IntakeSettings`] were refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeSettingsError(String);

impl fmt::Display for IntakeSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IntakeSettingsError {}

impl IntakeSettingsError {
    /// An error with `detail`, prefixed so it names the document.
    fn new(detail: impl fmt::Display) -> Self {
        Self(format!("intake settings: {detail}"))
    }
}

impl IntakeSettings {
    /// Settings with every default and `patron` holding every book.
    #[must_use]
    pub fn for_patron(patron: i64) -> Self {
        Self {
            settle_seconds: DEFAULT_SETTLE_SECONDS,
            scan_interval_seconds: DEFAULT_SCAN_INTERVAL_SECONDS,
            after_ingest: AfterIngest::Move,
            attribute_to: IntakeAttribution::Owner,
            patron: Some(patron),
            ignore: Vec::new(),
            prune_imported_days: None,
        }
    }

    /// Checks every bound and the rules between fields.
    ///
    /// # Errors
    ///
    /// Returns an [`IntakeSettingsError`] naming the first broken bound: a
    /// number out of range, a missing or unwanted `patron`, a bad ignore
    /// pattern, or `prune_imported_days` without `after_ingest = move`.
    pub fn validate(&self) -> Result<(), IntakeSettingsError> {
        if !(MIN_SETTLE_SECONDS..=MAX_SETTLE_SECONDS).contains(&self.settle_seconds) {
            return Err(IntakeSettingsError::new(format!(
                "settle_seconds must be {MIN_SETTLE_SECONDS} to {MAX_SETTLE_SECONDS}"
            )));
        }
        if !(MIN_SCAN_INTERVAL_SECONDS..=MAX_SCAN_INTERVAL_SECONDS)
            .contains(&self.scan_interval_seconds)
        {
            return Err(IntakeSettingsError::new(format!(
                "scan_interval_seconds must be {MIN_SCAN_INTERVAL_SECONDS} to {MAX_SCAN_INTERVAL_SECONDS}"
            )));
        }
        match (self.attribute_to, self.patron) {
            (IntakeAttribution::Owner, None) => {
                return Err(IntakeSettingsError::new(
                    "attribute_to = owner needs a patron user id",
                ))
            }
            (IntakeAttribution::Owner, Some(id)) if id < 1 => {
                return Err(IntakeSettingsError::new(
                    "patron must be a positive user id",
                ))
            }
            (IntakeAttribution::Subfolder, Some(_)) => {
                return Err(IntakeSettingsError::new(
                    "patron is set by the subfolder name when attribute_to = subfolder",
                ))
            }
            (IntakeAttribution::Owner, Some(_)) | (IntakeAttribution::Subfolder, None) => {}
        }
        if self.ignore.len() > MAX_IGNORE_PATTERNS {
            return Err(IntakeSettingsError::new(format!(
                "at most {MAX_IGNORE_PATTERNS} ignore patterns"
            )));
        }
        for (index, pattern) in self.ignore.iter().enumerate() {
            validate_pattern(pattern)
                .map_err(|why| IntakeSettingsError::new(format!("ignore[{index}]: {why}")))?;
            if self.ignore[..index].contains(pattern) {
                return Err(IntakeSettingsError::new(format!(
                    "ignore[{index}] repeats an earlier pattern"
                )));
            }
        }
        if let Some(days) = self.prune_imported_days {
            if !(1..=MAX_PRUNE_IMPORTED_DAYS).contains(&days) {
                return Err(IntakeSettingsError::new(format!(
                    "prune_imported_days must be 1 to {MAX_PRUNE_IMPORTED_DAYS}"
                )));
            }
            match self.after_ingest {
                AfterIngest::Move => {}
                AfterIngest::Delete | AfterIngest::Leave => {
                    return Err(IntakeSettingsError::new(
                        "prune_imported_days needs after_ingest = move",
                    ))
                }
            }
        }
        Ok(())
    }

    /// True when the file or folder name `name` (one path segment) is never
    /// part of an item: a dot name, a built-in pattern, an extra pattern,
    /// or one of the host's own folders at the root.
    ///
    /// # Arguments
    ///
    /// * `name` - One path segment.
    /// * `at_root` - True when `name` sits directly under the folder's root,
    ///   where `_imported` and `_failed` live.
    #[must_use]
    pub fn ignores(&self, name: &str, at_root: bool) -> bool {
        if name.starts_with('.') {
            return true;
        }
        if at_root && (name == IMPORTED_FOLDER || name == FAILED_FOLDER) {
            return true;
        }
        BUILTIN_IGNORE_PATTERNS
            .iter()
            .copied()
            .chain(self.ignore.iter().map(String::as_str))
            .any(|pattern| glob_matches(pattern, name))
    }
}

/// Checks one ignore pattern's bounds.
fn validate_pattern(pattern: &str) -> Result<(), &'static str> {
    if pattern.is_empty() {
        return Err("a pattern may not be empty");
    }
    if pattern.len() > MAX_IGNORE_PATTERN_BYTES {
        return Err("a pattern is at most 128 bytes");
    }
    if pattern.contains('/') || pattern.contains('\\') {
        return Err("a pattern matches one name and may not hold a path separator");
    }
    if pattern.chars().any(char::is_control) {
        return Err("a pattern may not hold control characters");
    }
    Ok(())
}

/// True when `name` matches `pattern`, where `*` matches any run of
/// characters (empty included) and `?` exactly one, compared without ASCII
/// case. Runs in time proportional to the product of the two lengths.
#[must_use]
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let name: Vec<char> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
    // Iterative matcher with one backtrack point: the last `*` seen and the
    // name position it was tried at.
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            Some(_) | None => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Reads and validates the settings an operator sent, or a stored
/// `core.intake` body.
///
/// # Errors
///
/// Returns an [`IntakeSettingsError`] for a body over
/// [`MAX_INTAKE_SETTINGS_BYTES`], bytes that are not one JSON object, an
/// unknown or repeated key, a value of the wrong type, or settings that fail
/// [`IntakeSettings::validate`].
pub fn parse_intake_settings(body: &[u8]) -> Result<IntakeSettings, IntakeSettingsError> {
    if body.len() > MAX_INTAKE_SETTINGS_BYTES {
        return Err(IntakeSettingsError::new(format!(
            "body is over {MAX_INTAKE_SETTINGS_BYTES} bytes"
        )));
    }
    let fields = serde_json::from_slice::<UniqueObject>(body)
        .map_err(IntakeSettingsError::new)?
        .0;
    let mut settings = IntakeSettings {
        settle_seconds: DEFAULT_SETTLE_SECONDS,
        scan_interval_seconds: DEFAULT_SCAN_INTERVAL_SECONDS,
        after_ingest: AfterIngest::Move,
        attribute_to: IntakeAttribution::Owner,
        patron: None,
        ignore: Vec::new(),
        prune_imported_days: None,
    };
    for (key, value) in fields {
        match key.as_str() {
            "settle_seconds" => settings.settle_seconds = read_u32(&key, &value)?,
            "scan_interval_seconds" => settings.scan_interval_seconds = read_u32(&key, &value)?,
            "after_ingest" => {
                settings.after_ingest = read_name(&key, &value, AfterIngest::parse)?;
            }
            "attribute_to" => {
                settings.attribute_to = read_name(&key, &value, IntakeAttribution::parse)?;
            }
            "patron" => settings.patron = read_optional(&value, |v| read_i64(&key, v))?,
            "ignore" => settings.ignore = read_patterns(&value)?,
            "prune_imported_days" => {
                settings.prune_imported_days = read_optional(&value, |v| read_u16(&key, v))?;
            }
            other => {
                return Err(IntakeSettingsError::new(format!("unknown field `{other}`")));
            }
        }
    }
    settings.validate()?;
    Ok(settings)
}

/// A JSON object read with every value kept as JSON, refusing a repeated
/// key and anything that is not an object.
struct UniqueObject(BTreeMap<String, Value>);

impl<'de> Deserialize<'de> for UniqueObject {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// Visits the top-level object.
        struct ObjectVisitor;

        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = UniqueObject;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UniqueObject, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value = map.next_value::<UniqueValue>()?.0;
                    if fields.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!("duplicate field `{key}`")));
                    }
                }
                Ok(UniqueObject(fields))
            }
        }

        de.deserialize_map(ObjectVisitor)
    }
}

/// Any JSON value, refusing an object that repeats a key at any depth.
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// Visits one value of any kind.
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = UniqueValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::Bool(v)))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::from(v)))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::from(v)))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::from(v)))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::String(v.to_string())))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::String(v)))
            }

            fn visit_unit<E: de::Error>(self) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::Null))
            }

            fn visit_none<E: de::Error>(self) -> Result<UniqueValue, E> {
                Ok(UniqueValue(Value::Null))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<UniqueValue, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<UniqueValue>()? {
                    items.push(item.0);
                }
                Ok(UniqueValue(Value::Array(items)))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<UniqueValue, A::Error> {
                let object = ObjectOf(map).read()?;
                Ok(UniqueValue(Value::Object(object)))
            }
        }

        /// Reads a nested object with the same duplicate check.
        struct ObjectOf<A>(A);

        impl<'de, A: MapAccess<'de>> ObjectOf<A> {
            /// Reads every entry, refusing a repeated key.
            fn read(mut self) -> Result<serde_json::Map<String, Value>, A::Error> {
                let mut object = serde_json::Map::new();
                while let Some(key) = self.0.next_key::<String>()? {
                    let value = self.0.next_value::<UniqueValue>()?.0;
                    if object.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!("duplicate field `{key}`")));
                    }
                }
                Ok(object)
            }
        }

        de.deserialize_any(ValueVisitor)
    }
}

/// Reads `value` as an unsigned 32-bit whole number.
fn read_u32(key: &str, value: &Value) -> Result<u32, IntakeSettingsError> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| IntakeSettingsError::new(format!("{key} must be a whole number")))
}

/// Reads `value` as an unsigned 16-bit whole number.
fn read_u16(key: &str, value: &Value) -> Result<u16, IntakeSettingsError> {
    value
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| IntakeSettingsError::new(format!("{key} must be a whole number")))
}

/// Reads `value` as a signed 64-bit whole number.
fn read_i64(key: &str, value: &Value) -> Result<i64, IntakeSettingsError> {
    value
        .as_i64()
        .ok_or_else(|| IntakeSettingsError::new(format!("{key} must be a whole number")))
}

/// Reads `value` as one of an enum's wire names.
fn read_name<T>(
    key: &str,
    value: &Value,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T, IntakeSettingsError> {
    value
        .as_str()
        .and_then(parse)
        .ok_or_else(|| IntakeSettingsError::new(format!("{key} has an unknown value")))
}

/// Reads `value` with `read`, or `None` for JSON `null`.
fn read_optional<T>(
    value: &Value,
    read: impl Fn(&Value) -> Result<T, IntakeSettingsError>,
) -> Result<Option<T>, IntakeSettingsError> {
    if value.is_null() {
        Ok(None)
    } else {
        read(value).map(Some)
    }
}

/// Reads the `ignore` array of strings.
fn read_patterns(value: &Value) -> Result<Vec<String>, IntakeSettingsError> {
    let items = value
        .as_array()
        .ok_or_else(|| IntakeSettingsError::new("ignore must be an array of strings"))?;
    if items.len() > MAX_IGNORE_PATTERNS {
        return Err(IntakeSettingsError::new(format!(
            "at most {MAX_IGNORE_PATTERNS} ignore patterns"
        )));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| IntakeSettingsError::new("ignore must be an array of strings"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_needs_a_patron() {
        assert!(parse_intake_settings(b"{}").is_err());
        let settings = parse_intake_settings(br#"{"patron": 3}"#).unwrap();
        assert_eq!(settings, IntakeSettings::for_patron(3));
    }

    #[test]
    fn subfolder_refuses_a_patron() {
        let settings = parse_intake_settings(br#"{"attribute_to": "subfolder"}"#).unwrap();
        assert_eq!(settings.attribute_to, IntakeAttribution::Subfolder);
        assert!(parse_intake_settings(br#"{"attribute_to": "subfolder", "patron": 2}"#).is_err());
    }

    #[test]
    fn bounds_and_shapes_are_enforced() {
        for body in [
            br#"{"patron": 1, "settle_seconds": 4}"#.as_slice(),
            br#"{"patron": 1, "settle_seconds": 86401}"#,
            br#"{"patron": 1, "scan_interval_seconds": 29}"#,
            br#"{"patron": 0}"#,
            br#"{"patron": 1, "patron": 2}"#,
            br#"{"patron": 1, "after_ingest": "copy"}"#,
            br#"{"patron": 1, "ignore": ["a/b"]}"#,
            br#"{"patron": 1, "ignore": [""]}"#,
            br#"{"patron": 1, "ignore": ["x", "x"]}"#,
            br#"{"patron": 1, "after_ingest": "leave", "prune_imported_days": 3}"#,
            br#"{"patron": 1, "prune_imported_days": 0}"#,
            br#"{"patron": 1, "colour": "red"}"#,
            br#"[1]"#,
            br#"{"patron": "1"}"#,
        ] {
            assert!(
                parse_intake_settings(body).is_err(),
                "accepted {}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn stored_settings_read_back() {
        let settings = IntakeSettings {
            settle_seconds: 120,
            scan_interval_seconds: 600,
            after_ingest: AfterIngest::Move,
            attribute_to: IntakeAttribution::Owner,
            patron: Some(7),
            ignore: vec!["*.nzb".to_string()],
            prune_imported_days: Some(30),
        };
        let body = serde_json::to_vec(&settings).unwrap();
        assert_eq!(parse_intake_settings(&body).unwrap(), settings);
    }

    #[test]
    fn ignores_temporaries_dot_names_and_host_folders() {
        let settings = IntakeSettings {
            ignore: vec!["*.NZB".to_string()],
            ..IntakeSettings::for_patron(1)
        };
        for name in [
            ".DS_Store",
            ".syncthing.book.m4b.tmp",
            "book.m4b.part",
            "Book.CRDOWNLOAD",
            "~lock",
            "Thumbs.db",
            "release.nzb",
        ] {
            assert!(settings.ignores(name, false), "{name}");
        }
        assert!(settings.ignores("_imported", true));
        assert!(!settings.ignores("_imported", false));
        assert!(!settings.ignores("Book One.m4b", true));
    }

    #[test]
    fn glob_matches_star_and_question_mark() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a*c", "abbbc"));
        assert!(glob_matches("a?c", "abc"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(glob_matches("*.m4b", "x.M4B"));
        assert!(!glob_matches("*.m4b", "x.m4a"));
        assert!(glob_matches("**a**", "bab"));
    }
}
