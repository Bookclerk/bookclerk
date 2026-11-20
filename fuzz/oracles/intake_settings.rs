//! Shared oracle for the `intake_settings` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A drop folder's settings reach [`parse_intake_settings`] from an
//! operator over the web API (`PUT /api/config/plugin-instances/{id}/intake`)
//! and from a stored `core.intake` document. The input is the request body.
//! The invariants, checked against an independent reading built on
//! `serde_json::Value` and a separate duplicate-key scan:
//!
//! - the parser accepts exactly the bodies the oracle accepts: at most
//!   16 KiB, one object with only the documented keys, no key repeated at
//!   any level, whole numbers in range (`settle_seconds` 5 to 86,400,
//!   `scan_interval_seconds` 30 to 86,400, `prune_imported_days` 1 to
//!   3,650 and only with `after_ingest = move`), the documented names for
//!   `after_ingest` and `attribute_to`, a positive `patron` exactly when
//!   `attribute_to` is `owner`, and at most 64 distinct ignore patterns of
//!   1 to 128 bytes with no path separator or control character;
//! - accepted settings equal the oracle's reading field by field and
//!   survive a JSON round trip.

use std::collections::BTreeSet;
use std::fmt;

use bookclerk_config::{parse_intake_settings, AfterIngest, IntakeAttribution};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: past the 16 KiB body cap.
pub const MAX_INPUT_BYTES: usize = 20 * 1024;

/// Walks any JSON value and reports whether some object repeats a key.
struct Duplicates;

impl<'de> Visitor<'de> for Duplicates {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let mut found = false;
        while let Some(inner) = seq.next_element::<DuplicateScan>()? {
            found |= inner.0;
        }
        Ok(found)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut keys = BTreeSet::new();
        let mut found = false;
        while let Some(key) = map.next_key::<String>()? {
            found |= !keys.insert(key);
            found |= map.next_value::<DuplicateScan>()?.0;
        }
        Ok(found)
    }
}

/// The result of a [`Duplicates`] walk.
struct DuplicateScan(bool);

impl<'de> serde::Deserialize<'de> for DuplicateScan {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(Duplicates).map(DuplicateScan)
    }
}

/// The oracle's reading of accepted settings.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    /// Quiet time, seconds.
    settle: u64,
    /// Listing interval, seconds.
    scan: u64,
    /// `move`, `delete` or `leave`.
    after: String,
    /// `owner` or `subfolder`.
    attribute: String,
    /// Owner patron.
    patron: Option<i64>,
    /// Extra ignore patterns.
    ignore: Vec<String>,
    /// Prune age, days.
    prune: Option<u64>,
}

/// Reads `data` the way the documentation describes, or `None` to refuse.
fn read(data: &[u8]) -> Option<Reading> {
    if data.len() > 16 * 1024 {
        return None;
    }
    let mut de = serde_json::Deserializer::from_slice(data);
    let dup = DuplicateScan::deserialize_from(&mut de)?;
    de.end().ok()?;
    if dup {
        return None;
    }
    let Value::Object(map) = serde_json::from_slice::<Value>(data).ok()? else {
        return None;
    };
    let mut reading = Reading {
        settle: 60,
        scan: 300,
        after: "move".into(),
        attribute: "owner".into(),
        patron: None,
        ignore: Vec::new(),
        prune: None,
    };
    for (key, value) in &map {
        match key.as_str() {
            "settle_seconds" => reading.settle = whole(value, u64::from(u32::MAX))?,
            "scan_interval_seconds" => reading.scan = whole(value, u64::from(u32::MAX))?,
            "after_ingest" => {
                let name = value.as_str()?;
                if !["move", "delete", "leave"].contains(&name) {
                    return None;
                }
                reading.after = name.into();
            }
            "attribute_to" => {
                let name = value.as_str()?;
                if !["owner", "subfolder"].contains(&name) {
                    return None;
                }
                reading.attribute = name.into();
            }
            "patron" => {
                reading.patron = if value.is_null() {
                    None
                } else {
                    Some(value.as_i64()?)
                }
            }
            "ignore" => {
                let items = value.as_array()?;
                let mut seen = BTreeSet::new();
                for item in items {
                    let text = item.as_str()?;
                    let ok = !text.is_empty()
                        && text.len() <= 128
                        && !text.contains('/')
                        && !text.contains('\\')
                        && !text.chars().any(char::is_control);
                    if !ok || !seen.insert(text.to_string()) {
                        return None;
                    }
                    reading.ignore.push(text.to_string());
                }
                if reading.ignore.len() > 64 {
                    return None;
                }
            }
            "prune_imported_days" => {
                reading.prune = if value.is_null() {
                    None
                } else {
                    Some(whole(value, u64::from(u16::MAX))?)
                }
            }
            _ => return None,
        }
    }
    let in_range = (5..=86_400).contains(&reading.settle)
        && (30..=86_400).contains(&reading.scan)
        && match reading.attribute.as_str() {
            "owner" => reading.patron.is_some_and(|id| id >= 1),
            _ => reading.patron.is_none(),
        }
        && reading
            .prune
            .is_none_or(|days| (1..=3_650).contains(&days) && reading.after == "move");
    in_range.then_some(reading)
}

/// A non-negative whole number no larger than `max`.
fn whole(value: &Value, max: u64) -> Option<u64> {
    value.as_u64().filter(|n| *n <= max)
}

impl DuplicateScan {
    /// Scans one value from `de`; `None` when it is not JSON.
    fn deserialize_from<'de, D: Deserializer<'de>>(de: D) -> Option<bool> {
        <DuplicateScan as serde::Deserialize>::deserialize(de)
            .ok()
            .map(|scan| scan.0)
    }
}

/// Checks the parser against the oracle on `data`.
///
/// # Panics
///
/// Panics when an invariant fails.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let parsed = parse_intake_settings(data);
    let expected = read(data);
    match (parsed, expected) {
        (Ok(settings), Some(reading)) => {
            assert_eq!(u64::from(settings.settle_seconds), reading.settle);
            assert_eq!(u64::from(settings.scan_interval_seconds), reading.scan);
            assert_eq!(settings.after_ingest.as_str(), reading.after);
            assert_eq!(settings.attribute_to.as_str(), reading.attribute);
            assert_eq!(settings.patron, reading.patron);
            assert_eq!(settings.ignore, reading.ignore);
            assert_eq!(settings.prune_imported_days.map(u64::from), reading.prune);
            assert_eq!(
                AfterIngest::parse(&reading.after),
                Some(settings.after_ingest)
            );
            assert_eq!(
                IntakeAttribution::parse(&reading.attribute),
                Some(settings.attribute_to)
            );
            let body = serde_json::to_vec(&settings).expect("settings serialize");
            assert_eq!(
                parse_intake_settings(&body).expect("round trip"),
                settings,
                "round trip changed the settings"
            );
        }
        (Err(_), None) => {}
        (Ok(settings), None) => panic!("parser accepted what the oracle refuses: {settings:?}"),
        (Err(err), Some(reading)) => {
            panic!("parser refused what the oracle accepts: {err} ({reading:?})")
        }
    }
}
