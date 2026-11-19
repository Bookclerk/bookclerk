//! Shared oracle for the `destination_filter` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A destination filter reaches [`parse_destination_filter`] from an
//! operator over the web API (`PUT /api/config/plugin-instances/{id}/filter`)
//! and from a stored `core.destination` document. The input is the request
//! body. The invariants, checked against an independent reading built on
//! `serde_json::Value` and a separate duplicate-key scan:
//!
//! - the parser accepts exactly the bodies the oracle accepts: an object
//!   with at most a `rules` array, each rule an object with exactly
//!   `field`, `value` and `effect`, no key repeated at any level, and the
//!   filter's bounds held (at most 64 rules; each value trimmed non-empty,
//!   at most 128 bytes, no NUL; a `patron` value a positive user id; a
//!   `media_kind` value `book` or `podcast`; a `format` value a copy
//!   format; a `quality` value a copy quality; no rule given twice);
//! - an accepted filter holds the trimmed rules in order, survives a JSON
//!   round trip, takes or refuses a set of sample books exactly as the
//!   oracle's reading of the book rules does, and receives or refuses a
//!   set of sample copies exactly as its reading of the copy rules
//!   (`format`, `quality`) does.

use std::collections::BTreeSet;
use std::fmt;

use bookclerk_config::{
    parse_destination_filter, FilterEffect, FilterField, OutputCopy, PlacementFacts,
    PlacementMediaKind,
};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: well past the largest filter.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

/// One rule as the oracle reads it: field name, trimmed value, include.
type OracleRule = (&'static str, String, bool);

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

/// The oracle's field name for a wire name.
fn field_name(name: &str) -> Option<&'static str> {
    match name {
        "patron" => Some("patron"),
        "storefront" => Some("storefront"),
        "account" => Some("account"),
        "media_kind" => Some("media_kind"),
        "format" => Some("format"),
        "quality" => Some("quality"),
        _ => None,
    }
}

/// True for the oracle's copy fields.
fn is_copy(field: &str) -> bool {
    field == "format" || field == "quality"
}

/// True when `value` (any case) names a copy quality: `original`, or a
/// bitrate `8k`..`320k` without leading zeros or a `v0`..`v9`, either
/// optionally followed by `-mono`.
fn oracle_quality(value: &str) -> bool {
    let value = value.to_lowercase();
    if value.len() > 16 {
        return false;
    }
    if value == "original" {
        return true;
    }
    let rate = value.strip_suffix("-mono").unwrap_or(&value);
    let bytes = rate.as_bytes();
    match bytes {
        [b'v', d] => d.is_ascii_digit(),
        [digits @ .., b'k'] if !digits.is_empty() && digits[0] != b'0' => {
            digits.iter().all(u8::is_ascii_digit)
                && digits.len() <= 3
                && rate[..rate.len() - 1]
                    .parse::<u32>()
                    .is_ok_and(|kbps| (8..=320).contains(&kbps))
        }
        _ => false,
    }
}

/// Reads `body` as a filter independently of the parser.
///
/// # Returns
///
/// The rules, or `None` when the body is not an acceptable filter.
fn oracle_read(body: &[u8]) -> Option<Vec<OracleRule>> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let scan = <DuplicateScan as serde::Deserialize>::deserialize(&mut de).ok()?;
    de.end().ok()?;
    if scan.0 {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    if object.keys().any(|key| key != "rules") {
        return None;
    }
    let rules = match object.get("rules") {
        None => return Some(Vec::new()),
        Some(rules) => rules.as_array()?,
    };
    if rules.len() > 64 {
        return None;
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for rule in rules {
        let rule = rule.as_object()?;
        if rule.len() != 3 {
            return None;
        }
        let field = field_name(rule.get("field")?.as_str()?)?;
        let value = rule.get("value")?.as_str()?.trim().to_string();
        let include = match rule.get("effect")?.as_str()? {
            "include" => true,
            "exclude" => false,
            _ => return None,
        };
        if value.is_empty() || value.len() > 128 || value.contains('\0') {
            return None;
        }
        let ok = match field {
            // A positive user id in canonical decimal: digits only, no
            // leading zero, within i64.
            "patron" => {
                value.bytes().all(|b| b.is_ascii_digit())
                    && !value.starts_with('0')
                    && value.parse::<i64>().is_ok()
            }
            "media_kind" => {
                value.eq_ignore_ascii_case("book") || value.eq_ignore_ascii_case("podcast")
            }
            "format" => {
                ["m4b", "mp3", "mp3_chapters", "original"].contains(&value.to_lowercase().as_str())
            }
            "quality" => oracle_quality(&value),
            _ => true,
        };
        if !ok || !seen.insert((field, value.to_lowercase(), include)) {
            return None;
        }
        out.push((field, value, include));
    }
    Some(out)
}

/// True when the oracle's `rule` matches the book `facts`.
fn oracle_matches(rule: &OracleRule, facts: &PlacementFacts) -> bool {
    let value = rule.1.as_str();
    match rule.0 {
        "patron" => facts.patrons.iter().any(|id| id.to_string() == value),
        "storefront" => facts.storefront.to_lowercase() == value.to_lowercase(),
        "account" => facts.account == value,
        _ => value.to_lowercase() == facts.media_kind.as_str(),
    }
}

/// True when the oracle's copy `rule` matches a copy written as its
/// `format` and `quality` names.
fn oracle_matches_copy(rule: &OracleRule, copy: (&str, &str)) -> bool {
    let value = rule.1.to_lowercase();
    match rule.0 {
        "format" => value == copy.0,
        _ => value == copy.1,
    }
}

/// The include and exclude decision over `rules` for one item.
fn oracle_decide(rules: &[&OracleRule], matches: impl Fn(&OracleRule) -> bool) -> bool {
    let refused = rules.iter().any(|rule| !rule.2 && matches(rule));
    let includes: Vec<&&OracleRule> = rules.iter().filter(|rule| rule.2).collect();
    !refused && (includes.is_empty() || includes.iter().any(|rule| matches(rule)))
}

/// Sample copies the accepted filter is asked about, by their names.
const COPY_SAMPLES: &[(&str, &str)] = &[
    ("m4b", "original"),
    ("mp3", "64k-mono"),
    ("mp3", "v2"),
    ("mp3_chapters", "128k"),
    ("original", "original"),
];

/// Sample books the accepted filter is asked about.
fn samples() -> Vec<PlacementFacts> {
    vec![
        PlacementFacts {
            patrons: vec![1, 3],
            storefront: "audible".into(),
            account: "acct".into(),
            media_kind: PlacementMediaKind::Book,
        },
        PlacementFacts {
            patrons: Vec::new(),
            storefront: "Libro".into(),
            account: "a".into(),
            media_kind: PlacementMediaKind::Podcast,
        },
    ]
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The request body.
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
    let expected = oracle_read(data);
    match parse_destination_filter(data) {
        Ok(filter) => {
            let rules =
                expected.unwrap_or_else(|| panic!("accepted {:?}", String::from_utf8_lossy(data)));
            let read: Vec<OracleRule> = filter
                .rules
                .iter()
                .map(|rule| {
                    let field = match rule.field {
                        FilterField::Patron => "patron",
                        FilterField::Storefront => "storefront",
                        FilterField::Account => "account",
                        FilterField::MediaKind => "media_kind",
                        FilterField::Format => "format",
                        FilterField::Quality => "quality",
                    };
                    (
                        field,
                        rule.value.clone(),
                        rule.effect == FilterEffect::Include,
                    )
                })
                .collect();
            assert_eq!(read, rules, "the rules differ from the oracle's");
            let json = serde_json::to_vec(&filter).expect("a filter serializes");
            assert_eq!(
                parse_destination_filter(&json).expect("a stored filter reads back"),
                filter
            );
            let book_rules: Vec<&OracleRule> =
                rules.iter().filter(|rule| !is_copy(rule.0)).collect();
            let copy_rules: Vec<&OracleRule> =
                rules.iter().filter(|rule| is_copy(rule.0)).collect();
            for facts in samples() {
                let taken = oracle_decide(&book_rules, |rule| oracle_matches(rule, &facts));
                assert_eq!(filter.accepts(&facts), taken, "{facts:?}");
            }
            for &(format, quality) in COPY_SAMPLES {
                let copy =
                    OutputCopy::parse(&format!("{format}:{quality}")).expect("a sample copy reads");
                let taken = oracle_decide(&copy_rules, |rule| {
                    oracle_matches_copy(rule, (format, quality))
                });
                assert_eq!(filter.accepts_copy(&copy), taken, "{format}:{quality}");
            }
            assert_eq!(filter.has_copy_rules(), !copy_rules.is_empty());
        }
        Err(_) => {
            assert!(
                expected.is_none(),
                "refused {:?}, which is a filter",
                String::from_utf8_lossy(data)
            );
        }
    }
}
