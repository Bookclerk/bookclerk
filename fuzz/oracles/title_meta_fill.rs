//! Shared oracle for the `title_meta_fill` fuzz target and the
//! `bookclerk-discover` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is a title-meta query from a signed-in user (the body of
//! `POST /api/discover/title-meta` and each entry of the batch route):
//! [`check_title_meta_query`] decides whether it is served, and
//! [`plan_title_meta_fill`] turns its `store_editions` and ISBN into the
//! storefront catalog-detail lookups the host makes on the user's behalf.
//!
//! Input layout: the first byte holds two capability bits for each of four
//! fake stores (`libro`, `graphicaudio`, `chirp`, `audible`, low bits
//! first: `detailByProductId`, then `detailByIsbn`); the rest is the JSON
//! body.
//!
//! The invariant, checked against this file's own reading of the body: an
//! object body is served exactly when its known fields have the documented
//! types (strings for `title` and `region`; string or `null` for `authors`,
//! `asin`, `isbn` and `narrators`; a 64-bit integer or `null` for
//! `length_minutes`; an array of `{source, product_id}` string objects for
//! `store_editions`), no known field is written twice, it names at most 16
//! editions, and its trimmed title, ASIN and ISBN are not all empty. A
//! served body plans at most three lookups: first, in edition order, at most
//! two product-id lookups at listing stores that declared
//! `detailByProductId` (trimmed non-empty ids, each store and id once), then
//! ISBN lookups at the stores that declared `detailByIsbn` in registry
//! order, by the trimmed ISBN or else an ISBN-shaped trimmed ASIN, skipping
//! a store already asked for that id. Positional (array) forms of the body
//! or of an edition are left to the typed parser; their plan is still
//! checked.

use std::collections::HashSet;
use std::fmt;

use bookclerk_discover::{
    check_title_meta_query, plan_title_meta_fill, FillLookup, FillStoreLookups, TitleMetaQuery,
};
use serde::de::{DeserializeSeed, Deserializer as _, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// Largest input worth checking; a real query is well under 2 KiB.
pub const MAX_INPUT_BYTES: usize = 4096;

/// Most editions the route documents per query.
const DOCUMENTED_MAX_EDITIONS: usize = 16;

/// Most lookups one fill documents.
const DOCUMENTED_MAX_LOOKUPS: usize = 3;

/// Most product-id lookups one fill documents.
const DOCUMENTED_MAX_PRODUCT_LOOKUPS: usize = 2;

/// The fake stores, in registry order.
const STORES: [&str; 4] = ["libro", "graphicaudio", "chirp", "audible"];

/// One fake store and what it declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Store {
    /// The store's id, also its plugin key.
    id: &'static str,
    /// Declared `detailByProductId`.
    by_product_id: bool,
    /// Declared `detailByIsbn`.
    by_isbn: bool,
}

/// A known field's value as written.
#[derive(Debug, Clone, PartialEq)]
enum Scalar {
    /// `null`.
    Null,
    /// A string.
    Str(String),
    /// An integer that fits in an `i64`.
    Int(i64),
    /// Anything else (booleans, other numbers, objects, arrays).
    Other,
}

/// An edition as written.
#[derive(Debug, Clone, PartialEq)]
enum Edition {
    /// An object form; `None` when a field is missing, repeated or not a
    /// string.
    Object(Option<(String, String)>),
    /// A positional (array) form.
    Positional,
}

/// The body as this oracle reads it.
#[derive(Debug, Default)]
struct Body {
    /// The body is a positional (array) form.
    positional: bool,
    /// A known field was written twice.
    repeated: bool,
    /// Known fields with their values, in the order written.
    fields: Vec<(String, Scalar)>,
    /// `store_editions` when written as an array.
    editions: Option<Vec<Edition>>,
    /// `store_editions` was written but is not an array.
    editions_bad: bool,
}

/// Top-level known fields besides `store_editions`.
const SCALAR_FIELDS: [&str; 7] = [
    "title",
    "authors",
    "asin",
    "isbn",
    "narrators",
    "length_minutes",
    "region",
];

/// Reads one known scalar field.
struct ScalarSeed;

impl<'de> DeserializeSeed<'de> for ScalarSeed {
    type Value = Scalar;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Scalar, D::Error> {
        /// Classifies one value.
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Scalar;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a value")
            }
            fn visit_unit<E>(self) -> Result<Scalar, E> {
                Ok(Scalar::Null)
            }
            fn visit_none<E>(self) -> Result<Scalar, E> {
                Ok(Scalar::Null)
            }
            fn visit_str<E>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar::Str(v.to_string()))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar::Int(v))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Scalar, E> {
                Ok(i64::try_from(v).map_or(Scalar::Other, Scalar::Int))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Scalar, E> {
                Ok(Scalar::Other)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Scalar, E> {
                Ok(Scalar::Other)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Scalar, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Scalar::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Scalar, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Scalar::Other)
            }
        }
        de.deserialize_any(V)
    }
}

/// Reads one edition.
struct EditionSeed;

impl<'de> DeserializeSeed<'de> for EditionSeed {
    type Value = Edition;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Edition, D::Error> {
        /// Reads an edition object's fields.
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Edition;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an edition")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Edition, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Edition::Positional)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Edition, A::Error> {
                let (mut source, mut product_id) = (Vec::new(), Vec::new());
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "source" => source.push(map.next_value_seed(ScalarSeed)?),
                        "product_id" => product_id.push(map.next_value_seed(ScalarSeed)?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                let pair = match (source.as_slice(), product_id.as_slice()) {
                    ([Scalar::Str(s)], [Scalar::Str(p)]) => Some((s.clone(), p.clone())),
                    _ => None,
                };
                Ok(Edition::Object(pair))
            }
        }
        de.deserialize_any(V)
    }
}

/// Reads the `store_editions` value.
struct EditionsSeed;

impl<'de> DeserializeSeed<'de> for EditionsSeed {
    type Value = Option<Vec<Edition>>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        de: D,
    ) -> Result<Option<Vec<Edition>>, D::Error> {
        /// Reads the array, or notes that it is not one.
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<Vec<Edition>>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("editions")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(edition) = seq.next_element_seed(EditionSeed)? {
                    out.push(edition);
                }
                Ok(Some(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(None)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
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
        }
        de.deserialize_any(V)
    }
}

/// Reads the whole body.
struct BodyVisitor;

impl<'de> Visitor<'de> for BodyVisitor {
    type Value = Body;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a query object")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Body, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Body {
            positional: true,
            ..Body::default()
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Body, A::Error> {
        let mut body = Body::default();
        let mut written = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let known = key == "store_editions" || SCALAR_FIELDS.contains(&key.as_str());
            if known && !written.insert(key.clone()) {
                body.repeated = true;
            }
            if key == "store_editions" {
                match map.next_value_seed(EditionsSeed)? {
                    Some(editions) => body.editions = Some(editions),
                    None => body.editions_bad = true,
                }
            } else if known {
                let value = map.next_value_seed(ScalarSeed)?;
                body.fields.push((key, value));
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(body)
    }
}

/// What the oracle expects the parser and the check to do.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// The body is refused (by the parser or by the check).
    Refused,
    /// Served, with this title, ASIN, ISBN and editions as written.
    Served {
        /// `title` as written (empty when absent).
        title: String,
        /// `asin` as written.
        asin: Option<String>,
        /// `isbn` as written.
        isbn: Option<String>,
        /// The editions as written (source, product id).
        editions: Vec<(String, String)>,
    },
    /// A positional form: acceptance is the typed parser's call.
    Deferred,
}

/// Applies the documented rules to the body.
fn expected(json: &[u8]) -> Expected {
    let mut de = serde_json::Deserializer::from_slice(json);
    let Ok(body) = de.deserialize_any(BodyVisitor) else {
        return Expected::Refused;
    };
    if de.end().is_err() {
        return Expected::Refused;
    }
    if body.positional {
        return Expected::Deferred;
    }
    if body.repeated || body.editions_bad {
        return Expected::Refused;
    }
    let (mut title, mut asin, mut isbn) = (String::new(), None, None);
    for (key, value) in &body.fields {
        let ok = match (key.as_str(), value) {
            ("title", Scalar::Str(s)) => {
                title = s.clone();
                true
            }
            ("region", Scalar::Str(_)) => true,
            ("length_minutes", Scalar::Int(_) | Scalar::Null) => true,
            ("asin", Scalar::Str(s)) => {
                asin = Some(s.clone());
                true
            }
            ("isbn", Scalar::Str(s)) => {
                isbn = Some(s.clone());
                true
            }
            ("authors" | "narrators" | "asin" | "isbn", Scalar::Null) => true,
            ("authors" | "narrators", Scalar::Str(_)) => true,
            _ => false,
        };
        if !ok {
            return Expected::Refused;
        }
    }
    let mut editions = Vec::new();
    for edition in body.editions.unwrap_or_default() {
        match edition {
            Edition::Positional => return Expected::Deferred,
            Edition::Object(None) => return Expected::Refused,
            Edition::Object(Some(pair)) => editions.push(pair),
        }
    }
    let blank = |v: &Option<String>| v.as_deref().is_none_or(|s| s.trim().is_empty());
    if editions.len() > DOCUMENTED_MAX_EDITIONS
        || (title.trim().is_empty() && blank(&asin) && blank(&isbn))
    {
        return Expected::Refused;
    }
    Expected::Served {
        title,
        asin,
        isbn,
        editions,
    }
}

/// True for an ISBN-10 or ISBN-13 once hyphens are dropped, checked here
/// byte by byte.
fn isbn_shaped(raw: &str) -> bool {
    let digits: Vec<u8> = raw.bytes().filter(|&b| b != b'-').collect();
    match digits.as_slice() {
        [body @ .., last] if digits.len() == 10 => {
            body.iter().all(u8::is_ascii_digit)
                && (last.is_ascii_digit() || *last == b'X' || *last == b'x')
        }
        all if all.len() == 13 => all.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

/// The plan the documented rules give, as (store id, lookup) pairs.
fn expected_plan(query: &TitleMetaQuery, stores: &[Store]) -> Vec<(&'static str, FillLookup)> {
    let mut plan = Vec::new();
    let mut asked: HashSet<(&str, String)> = HashSet::new();
    let mut products = 0;
    for edition in &query.store_editions {
        if products == DOCUMENTED_MAX_PRODUCT_LOOKUPS {
            break;
        }
        let id = edition.product_id.trim();
        let Some(store) = stores.iter().find(|s| s.id == edition.source.trim()) else {
            continue;
        };
        if id.is_empty() || !store.by_product_id || asked.contains(&(store.id, id.to_string())) {
            continue;
        }
        asked.insert((store.id, id.to_string()));
        plan.push((store.id, FillLookup::Product(id.to_string())));
        products += 1;
    }
    let isbn = match query.isbn.as_deref().map(str::trim) {
        Some(isbn) if !isbn.is_empty() => Some(isbn),
        _ => query
            .asin
            .as_deref()
            .map(str::trim)
            .filter(|a| isbn_shaped(a)),
    };
    if let Some(isbn) = isbn {
        for store in stores.iter().filter(|s| s.by_isbn) {
            if plan.len() == DOCUMENTED_MAX_LOOKUPS {
                break;
            }
            if asked.insert((store.id, isbn.to_string())) {
                plan.push((store.id, FillLookup::Isbn(isbn.to_string())));
            }
        }
    }
    plan
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The capability byte, then the query body.
///
/// # Returns
///
/// True when the query was served.
///
/// # Panics
///
/// When the check or the planner disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    let Some((&caps, json)) = data.split_first() else {
        return false;
    };
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let stores: Vec<Store> = STORES
        .iter()
        .enumerate()
        .map(|(i, &id)| Store {
            id,
            by_product_id: caps & (1 << (2 * i)) != 0,
            by_isbn: caps & (1 << (2 * i + 1)) != 0,
        })
        .collect();
    let parsed = serde_json::from_slice::<TitleMetaQuery>(json);
    let served = parsed
        .as_ref()
        .ok()
        .filter(|query| check_title_meta_query(query).is_ok());
    let shown = String::from_utf8_lossy(json);
    match expected(json) {
        Expected::Refused => {
            assert!(served.is_none(), "{shown:?} served: {parsed:?}");
            return false;
        }
        Expected::Served {
            title,
            asin,
            isbn,
            editions,
        } => {
            let query = served.unwrap_or_else(|| panic!("{shown:?} refused: {parsed:?}"));
            assert_eq!(query.title, title);
            assert_eq!(query.asin, asin);
            assert_eq!(query.isbn, isbn);
            let got: Vec<(String, String)> = query
                .store_editions
                .iter()
                .map(|e| (e.source.clone(), e.product_id.clone()))
                .collect();
            assert_eq!(got, editions);
        }
        Expected::Deferred => {}
    }
    let Some(query) = served else {
        return false;
    };
    let plan = plan_title_meta_fill(
        query,
        |id| stores.iter().find(|s| s.id == id).copied(),
        stores.iter().copied(),
        |s| FillStoreLookups {
            plugin_key: s.id.to_string(),
            by_product_id: s.by_product_id,
            by_isbn: s.by_isbn,
        },
    );
    let got: Vec<(&'static str, FillLookup)> = plan.into_iter().map(|(s, l)| (s.id, l)).collect();
    assert_eq!(got, expected_plan(query, &stores), "{shown:?}");
    true
}
