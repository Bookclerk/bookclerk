//! Shared oracle for the `output_copies` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A library's extra copies reach the host as the `copies` array of the
//! `core.output` document: the body of
//! `PUT /api/config/domains/core.output` and the stored row. The server
//! reads the array with `OutputCopy`'s serde derive and checks it with
//! [`validate_copies`] against the main output's copy; this target drives
//! the same two steps. The input's first byte picks the main output's copy
//! (see [`MAINS`]) and the rest is the array. The invariants, checked
//! against an independent reading built on `serde_json::Value` and a
//! separate duplicate-key scan:
//!
//! - the list is accepted exactly when the oracle accepts it: an array of
//!   at most 8 objects, each with exactly `format` and `quality` and no key
//!   repeated; a `format` one of `m4b`, `mp3`, `mp3_chapters`,
//!   `original` or `opus` as written; a `quality` that, trimmed and in any
//!   ASCII case, is `original`, a bitrate (16, 24, 32, 48, 64, 96, 128,
//!   160, 192, 256 or 320 kbit/s, no leading zero) or `v0`..`v9`, either
//!   optionally followed by `-mono`, at most 16 bytes; `original` for M4B
//!   and as-delivered copies; for MP3 copies a VBR level or one of the MP3
//!   encoder's rates (64 to 320); for Opus copies one of the Opus targets
//!   (16 to 128); no copy given twice and none the main output's copy;
//! - an accepted list holds the oracle's copies in order, in canonical
//!   form, and survives a JSON round trip.

use std::collections::BTreeSet;
use std::fmt;

use bookclerk_config::{validate_copies, OutputCopy};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: well past the longest valid list.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

/// The main output's copy the selector byte picks, as `format:quality`;
/// `None` is an output format the acquire pipeline refuses.
const MAINS: [Option<(&str, &str)>; 5] = [
    Some(("m4b", "original")),
    Some(("mp3", "v2")),
    Some(("mp3_chapters", "64k-mono")),
    Some(("opus", "64k")),
    None,
];

/// The MP3 encoder's constant rates, in kbit/s.
const MP3_RATES: [u32; 7] = [64, 96, 128, 160, 192, 256, 320];

/// The Opus targets, in kbit/s.
const OPUS_RATES: [u32; 7] = [16, 24, 32, 48, 64, 96, 128];

/// One copy as the oracle reads it: format name, canonical quality.
type OracleCopy = (String, String);

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

/// The canonical form of a quality name, or `None` when it is not one.
fn oracle_quality(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.len() > 16 {
        return None;
    }
    if value == "original" {
        return Some(value);
    }
    let rate = value.strip_suffix("-mono").unwrap_or(&value);
    let ok = match rate.as_bytes() {
        [b'v', d] => d.is_ascii_digit(),
        [digits @ .., b'k'] if !digits.is_empty() && digits[0] != b'0' => {
            digits.len() <= 3
                && digits.iter().all(u8::is_ascii_digit)
                && rate[..rate.len() - 1]
                    .parse::<u32>()
                    .is_ok_and(|kbps| MP3_RATES.contains(&kbps) || OPUS_RATES.contains(&kbps))
        }
        _ => false,
    };
    ok.then_some(value)
}

/// Reads `body` as a copy list beside the main copy `main`, independently
/// of the parser.
///
/// # Returns
///
/// The copies, or `None` when the list is not acceptable.
fn oracle_read(main: Option<(&str, &str)>, body: &[u8]) -> Option<Vec<OracleCopy>> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let scan = <DuplicateScan as serde::Deserialize>::deserialize(&mut de).ok()?;
    de.end().ok()?;
    if scan.0 {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let items = value.as_array()?;
    if items.len() > 8 {
        return None;
    }
    let main = main.map(|(format, quality)| (format.to_string(), quality.to_string()));
    let mut out: Vec<OracleCopy> = Vec::new();
    for item in items {
        let item = item.as_object()?;
        if item.len() != 2 {
            return None;
        }
        let format = item.get("format")?.as_str()?;
        if !["m4b", "mp3", "mp3_chapters", "original", "opus"].contains(&format) {
            return None;
        }
        let quality = oracle_quality(item.get("quality")?.as_str()?)?;
        let rate = quality.strip_suffix("-mono").unwrap_or(&quality);
        let kbps = rate.strip_suffix('k').and_then(|k| k.parse::<u32>().ok());
        let vbr = rate.starts_with('v');
        let fits = match format {
            "mp3" | "mp3_chapters" => vbr || kbps.is_some_and(|k| MP3_RATES.contains(&k)),
            "opus" => kbps.is_some_and(|k| OPUS_RATES.contains(&k)),
            _ => quality == "original",
        };
        if !fits {
            return None;
        }
        let copy = (format.to_string(), quality);
        if main.as_ref() == Some(&copy) || out.contains(&copy) {
            return None;
        }
        out.push(copy);
    }
    Some(out)
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The main copy's selector byte, then the `copies` array.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
// Oracle code: a disagreement is the finding, reported by panicking.
#[allow(clippy::expect_used, clippy::panic)]
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let main = MAINS[usize::from(selector) % MAINS.len()];
    let main_copy = main.map(|(format, quality)| {
        OutputCopy::parse(&format!("{format}:{quality}")).expect("a main copy reads")
    });
    let expected = oracle_read(main, body);
    let parsed = serde_json::from_slice::<Vec<OutputCopy>>(body)
        .ok()
        .filter(|copies| validate_copies(main_copy.as_ref(), copies).is_ok());
    match parsed {
        Some(copies) => {
            let copies_read = expected.unwrap_or_else(|| {
                panic!("accepted {:?}", String::from_utf8_lossy(body));
            });
            let read: Vec<OracleCopy> = copies
                .iter()
                .map(|copy| (copy.format.to_string(), copy.quality.to_string()))
                .collect();
            assert_eq!(read, copies_read, "the copies differ from the oracle's");
            let json = serde_json::to_vec(&copies).expect("a copy list serializes");
            let back: Vec<OutputCopy> =
                serde_json::from_slice(&json).expect("a stored copy list reads back");
            assert_eq!(back, copies);
        }
        None => assert!(
            expected.is_none(),
            "refused {:?}, which is a copy list",
            String::from_utf8_lossy(body)
        ),
    }
}
