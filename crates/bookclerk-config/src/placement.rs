//! Which books a storage destination takes (see
//! `docs/adr/storage-destinations.md`, decision 2).
//!
//! A destination's filter is built like a patron scope: a list of rules,
//! each a field, one value of that field, and whether it includes or
//! excludes the books that have it. With no include rules every book
//! passes; with any, a book must match at least one. An exclude rule always
//! wins over an include. A destination with no filter takes every book.
//!
//! The acquire job evaluates every destination's filter against the book's
//! [`PlacementFacts`] before it starts, and fails at once when no
//! destination takes the book.

use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;

/// Most rules one destination filter may hold.
pub const MAX_FILTER_RULES: usize = 64;

/// Longest rule value, in UTF-8 bytes.
pub const MAX_FILTER_VALUE_BYTES: usize = 128;

/// The book field a [`FilterRule`] matches one value of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterField {
    /// A patron whose store account holds the book, by user id (`3`).
    Patron,
    /// The storefront the book came from, by its plugin's alias
    /// (`audible`, `libro`), compared without case.
    Storefront,
    /// The store account that holds the book, by its id at the store.
    Account,
    /// What the book is: `book` or `podcast` (a podcast episode is a
    /// podcast).
    MediaKind,
}

impl FilterField {
    /// The field's name on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Patron => "patron",
            Self::Storefront => "storefront",
            Self::Account => "account",
            Self::MediaKind => "media_kind",
        }
    }
}

/// Whether a [`FilterRule`] lets matching books in or keeps them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterEffect {
    /// Matching books are taken (once any include rule exists, only books
    /// that match one are).
    Include,
    /// Matching books are refused, whatever the include rules say.
    Exclude,
}

/// One rule of a destination filter: a field, one value of it, and its
/// effect.
// Read by a strict hand-written reader (module `strict`); the serde
// attributes here shape the schema and match what that reader takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterRule {
    /// The field the rule matches.
    pub field: FilterField,
    /// The value: a user id for `patron`, `book` or `podcast` for
    /// `media_kind`, otherwise 1 to 128 bytes of text.
    #[schema(example = "audible")]
    pub value: String,
    /// Include or exclude.
    pub effect: FilterEffect,
}

/// Which books one destination takes.
// Read by a strict hand-written reader (module `strict`); the serde
// attributes here shape the schema and match what that reader takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DestinationFilter {
    /// Filter rules, at most 64. Empty means the destination takes every
    /// book.
    #[serde(default)]
    pub rules: Vec<FilterRule>,
}

/// What a book is, as a destination filter sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlacementMediaKind {
    /// A book (any form: narration, dramatisation, ebook and so on).
    Book,
    /// A podcast show or one of its episodes.
    Podcast,
}

impl PlacementMediaKind {
    /// The kind's name on the wire and in rule values.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Book => "book",
            Self::Podcast => "podcast",
        }
    }

    /// Reads a rule value (`book` or `podcast`, without case).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("book") {
            Some(Self::Book)
        } else if value.eq_ignore_ascii_case("podcast") {
            Some(Self::Podcast)
        } else {
            None
        }
    }

    /// The kind of a library title from its content kind label (`book`,
    /// `podcast` or `episode`): episodes and shows are podcasts, everything
    /// else is a book.
    #[must_use]
    pub fn from_content_kind(content_kind: &str) -> Self {
        match content_kind {
            "podcast" | "episode" => Self::Podcast,
            _ => Self::Book,
        }
    }
}

/// What a destination filter knows about one book when an acquire starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementFacts {
    /// Users linked to the store account that holds the book, ascending.
    /// Empty when the account is linked to nobody.
    pub patrons: Vec<i64>,
    /// The storefront plugin's alias (`audible`, `libro`, …).
    pub storefront: String,
    /// The holding account's id at that store.
    pub account: String,
    /// What the book is.
    pub media_kind: PlacementMediaKind,
}

/// Why a [`DestinationFilter`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError(String);

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FilterError {}

impl FilterRule {
    /// True when the book described by `facts` has this rule's value in
    /// the rule's field. The effect is not considered.
    #[must_use]
    pub fn matches(&self, facts: &PlacementFacts) -> bool {
        let value = self.value.trim();
        match self.field {
            FilterField::Patron => value
                .parse::<i64>()
                .is_ok_and(|id| facts.patrons.contains(&id)),
            FilterField::Storefront => facts.storefront.eq_ignore_ascii_case(value),
            FilterField::Account => facts.account == value,
            FilterField::MediaKind => PlacementMediaKind::parse(value) == Some(facts.media_kind),
        }
    }
}

impl DestinationFilter {
    /// A filter that takes every book: no rules.
    #[must_use]
    pub fn takes_everything(&self) -> bool {
        self.rules.is_empty()
    }

    /// True when the destination takes the book described by `facts`: no
    /// exclude rule matches, and either there is no include rule or one of
    /// them matches.
    #[must_use]
    pub fn accepts(&self, facts: &PlacementFacts) -> bool {
        let mut any_include = false;
        let mut included = false;
        for rule in &self.rules {
            match rule.effect {
                FilterEffect::Exclude => {
                    if rule.matches(facts) {
                        return false;
                    }
                }
                FilterEffect::Include => {
                    any_include = true;
                    included = included || rule.matches(facts);
                }
            }
        }
        !any_include || included
    }

    /// Checks the filter's bounds.
    ///
    /// # Errors
    ///
    /// Returns a [`FilterError`] for more than [`MAX_FILTER_RULES`] rules,
    /// an empty or over-long value, a value with a NUL, a `patron` value
    /// that is not a positive user id, a `media_kind` value other than
    /// `book` or `podcast`, or a rule given twice (the same field, value and
    /// effect, compared without case).
    pub fn validate(&self) -> Result<(), FilterError> {
        if self.rules.len() > MAX_FILTER_RULES {
            return Err(FilterError(format!(
                "a destination filter holds at most {MAX_FILTER_RULES} rules"
            )));
        }
        let mut seen = BTreeSet::new();
        for rule in &self.rules {
            let value = rule.value.trim();
            if value.is_empty() {
                return Err(FilterError("a filter rule needs a value".into()));
            }
            if value.len() > MAX_FILTER_VALUE_BYTES || value.contains('\u{0000}') {
                return Err(FilterError(format!(
                    "a filter rule value is at most {MAX_FILTER_VALUE_BYTES} bytes, with no NUL"
                )));
            }
            match rule.field {
                FilterField::Patron => {
                    if !value.parse::<i64>().is_ok_and(|id| id > 0) {
                        return Err(FilterError(format!(
                            "a patron rule takes a user id, not `{value}`"
                        )));
                    }
                }
                FilterField::MediaKind => {
                    if PlacementMediaKind::parse(value).is_none() {
                        return Err(FilterError(format!(
                            "a media_kind rule takes `book` or `podcast`, not `{value}`"
                        )));
                    }
                }
                FilterField::Storefront | FilterField::Account => {}
            }
            if !seen.insert((rule.field, value.to_lowercase(), rule.effect)) {
                return Err(FilterError(format!(
                    "the {} rule `{value}` is given twice",
                    rule.field.as_str()
                )));
            }
        }
        Ok(())
    }

    /// The filter with each value trimmed, as it is stored.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        for rule in &mut self.rules {
            rule.value = rule.value.trim().to_string();
        }
        self
    }
}

/// Parses a destination filter from its raw JSON bytes, sans IO, the way
/// the web API reads it: the strict JSON shape, then trimmed
/// ([`DestinationFilter::normalized`]) and checked
/// ([`DestinationFilter::validate`]).
///
/// # Errors
///
/// Returns a [`FilterError`] for bytes that are not the filter's JSON shape
/// (unknown or repeated keys, wrong types, a missing rule key, an unknown
/// field or effect name) or a filter that fails
/// [`DestinationFilter::validate`].
pub fn parse_destination_filter(body: &[u8]) -> Result<DestinationFilter, FilterError> {
    let filter = serde_json::from_slice::<DestinationFilter>(body)
        .map_err(|err| FilterError(format!("destination filter: {err}")))?
        .normalized();
    filter.validate()?;
    Ok(filter)
}

/// Strict JSON readers for the filter types.
///
/// A derived reader would also take a struct written as an array
/// (`["storefront", "audible", "include"]`) and a unit variant written as
/// an object. Filters come from operators over the web API, so the readers
/// here take only the documented shape: objects for the structs, with no
/// unknown, repeated or (for a rule) missing keys, and strings for the
/// field and effect names. The `destination_filter` fuzz target checks them
/// against an independent reading.
mod strict {
    use std::fmt;

    use serde::de::{self, Deserializer, MapAccess, Visitor};
    use serde::Deserialize;

    use super::{DestinationFilter, FilterEffect, FilterField, FilterRule};

    /// Reads one of the names of a unit enum from a JSON string.
    struct Name<T: 'static> {
        /// What the reader expects, for errors.
        what: &'static str,
        /// The accepted names and their values.
        names: &'static [(&'static str, T)],
        /// The accepted names alone, for errors.
        expected: &'static [&'static str],
    }

    impl<T: Copy> Visitor<'_> for Name<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.what)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
            self.names
                .iter()
                .find(|(name, _)| *name == value)
                .map(|(_, found)| *found)
                .ok_or_else(|| E::unknown_variant(value, self.expected))
        }
    }

    impl<'de> Deserialize<'de> for FilterField {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_str(Name {
                what: "a filter field name",
                names: &[
                    ("patron", FilterField::Patron),
                    ("storefront", FilterField::Storefront),
                    ("account", FilterField::Account),
                    ("media_kind", FilterField::MediaKind),
                ],
                expected: &["patron", "storefront", "account", "media_kind"],
            })
        }
    }

    impl<'de> Deserialize<'de> for FilterEffect {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_str(Name {
                what: "include or exclude",
                names: &[
                    ("include", FilterEffect::Include),
                    ("exclude", FilterEffect::Exclude),
                ],
                expected: &["include", "exclude"],
            })
        }
    }

    /// Stores one key's value, refusing a key written twice.
    fn once<'de, A: MapAccess<'de>, T: Deserialize<'de>>(
        map: &mut A,
        slot: &mut Option<T>,
        key: &'static str,
    ) -> Result<(), A::Error> {
        if slot.is_some() {
            return Err(de::Error::duplicate_field(key));
        }
        *slot = Some(map.next_value()?);
        Ok(())
    }

    /// Reads a [`FilterRule`] from an object.
    struct RuleVisitor;

    impl<'de> Visitor<'de> for RuleVisitor {
        type Value = FilterRule;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a filter rule object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<FilterRule, A::Error> {
            const KEYS: &[&str] = &["field", "value", "effect"];
            let (mut field, mut value, mut effect) = (None, None, None);
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "field" => once(&mut map, &mut field, "field")?,
                    "value" => once(&mut map, &mut value, "value")?,
                    "effect" => once(&mut map, &mut effect, "effect")?,
                    other => return Err(de::Error::unknown_field(other, KEYS)),
                }
            }
            Ok(FilterRule {
                field: field.ok_or_else(|| de::Error::missing_field("field"))?,
                value: value.ok_or_else(|| de::Error::missing_field("value"))?,
                effect: effect.ok_or_else(|| de::Error::missing_field("effect"))?,
            })
        }
    }

    impl<'de> Deserialize<'de> for FilterRule {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_map(RuleVisitor)
        }
    }

    /// Reads a [`DestinationFilter`] from an object.
    struct FilterVisitor;

    impl<'de> Visitor<'de> for FilterVisitor {
        type Value = DestinationFilter;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a destination filter object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<DestinationFilter, A::Error> {
            let mut rules = None;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "rules" => once(&mut map, &mut rules, "rules")?,
                    other => return Err(de::Error::unknown_field(other, &["rules"])),
                }
            }
            Ok(DestinationFilter {
                rules: rules.unwrap_or_default(),
            })
        }
    }

    impl<'de> Deserialize<'de> for DestinationFilter {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_map(FilterVisitor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A book held by user 3's Audible account `acct-1`.
    fn audible_book() -> PlacementFacts {
        PlacementFacts {
            patrons: vec![3],
            storefront: "audible".into(),
            account: "acct-1".into(),
            media_kind: PlacementMediaKind::Book,
        }
    }

    /// A rule from its parts.
    fn rule(field: FilterField, value: &str, effect: FilterEffect) -> FilterRule {
        FilterRule {
            field,
            value: value.into(),
            effect,
        }
    }

    #[test]
    fn no_rules_take_every_book() {
        let filter = DestinationFilter::default();
        assert!(filter.takes_everything());
        assert!(filter.accepts(&audible_book()));
    }

    #[test]
    fn include_rules_take_only_matching_books() {
        let filter = DestinationFilter {
            rules: vec![
                rule(FilterField::Storefront, "LIBRO", FilterEffect::Include),
                rule(FilterField::Patron, "3", FilterEffect::Include),
            ],
        };
        assert!(filter.accepts(&audible_book()), "patron 3 matches");
        let other = PlacementFacts {
            patrons: vec![4],
            ..audible_book()
        };
        assert!(!filter.accepts(&other), "neither include matches");
        let libro = PlacementFacts {
            storefront: "libro".into(),
            ..other
        };
        assert!(filter.accepts(&libro), "storefront compared without case");
    }

    #[test]
    fn an_exclude_wins_over_an_include() {
        let filter = DestinationFilter {
            rules: vec![
                rule(FilterField::Storefront, "audible", FilterEffect::Include),
                rule(FilterField::MediaKind, "book", FilterEffect::Exclude),
            ],
        };
        assert!(!filter.accepts(&audible_book()));
        let podcast = PlacementFacts {
            media_kind: PlacementMediaKind::Podcast,
            ..audible_book()
        };
        assert!(filter.accepts(&podcast));
    }

    #[test]
    fn accounts_match_exactly() {
        let filter = DestinationFilter {
            rules: vec![rule(FilterField::Account, "ACCT-1", FilterEffect::Include)],
        };
        assert!(!filter.accepts(&audible_book()));
    }

    #[test]
    fn episodes_and_shows_are_podcasts() {
        assert_eq!(
            PlacementMediaKind::from_content_kind("episode"),
            PlacementMediaKind::Podcast
        );
        assert_eq!(
            PlacementMediaKind::from_content_kind("podcast"),
            PlacementMediaKind::Podcast
        );
        assert_eq!(
            PlacementMediaKind::from_content_kind("book"),
            PlacementMediaKind::Book
        );
        assert_eq!(PlacementMediaKind::Podcast.as_str(), "podcast");
    }

    #[test]
    fn the_strict_reader_takes_the_documented_shape() {
        let filter = parse_destination_filter(
            br#"{"rules":[{"field":"media_kind","value":" podcast ","effect":"exclude"}]}"#,
        )
        .unwrap();
        assert_eq!(
            filter.rules,
            vec![rule(
                FilterField::MediaKind,
                "podcast",
                FilterEffect::Exclude
            )]
        );
        assert_eq!(
            parse_destination_filter(b"{}").unwrap(),
            DestinationFilter::default()
        );
        let json = serde_json::to_vec(&filter).unwrap();
        assert_eq!(parse_destination_filter(&json).unwrap(), filter);
    }

    #[test]
    fn the_strict_reader_refuses_other_shapes() {
        for body in [
            &br#"[]"#[..],
            br#"{"rules":[["storefront","audible","include"]]}"#,
            br#"{"rules":[],"rules":[]}"#,
            br#"{"rules":[{"field":"storefront","value":"a"}]}"#,
            br#"{"rules":[{"field":"storefront","value":"a","effect":"include","x":1}]}"#,
            br#"{"rules":[{"field":{"storefront":null},"value":"a","effect":"include"}]}"#,
            br#"{"rules":[{"field":"genre","value":"a","effect":"include"}]}"#,
            br#"{"rules":[{"field":"patron","value":"0","effect":"include"}]}"#,
            br#"{"rules":[{"field":"patron","value":"me","effect":"include"}]}"#,
            br#"{"rules":[{"field":"media_kind","value":"ebook","effect":"include"}]}"#,
            br#"{"rules":[{"field":"account","value":"  ","effect":"include"}]}"#,
            br#"{"rules":[{"field":"account","value":"a","effect":"include"},{"field":"account","value":"A","effect":"include"}]}"#,
            br#"{"other":1}"#,
        ] {
            assert!(
                parse_destination_filter(body).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let long = format!(
            r#"{{"rules":[{{"field":"account","value":"{}","effect":"include"}}]}}"#,
            "a".repeat(MAX_FILTER_VALUE_BYTES + 1)
        );
        assert!(parse_destination_filter(long.as_bytes()).is_err());
        let many = format!(
            r#"{{"rules":[{}]}}"#,
            (0..=MAX_FILTER_RULES)
                .map(|n| format!(r#"{{"field":"account","value":"a{n}","effect":"include"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse_destination_filter(many.as_bytes()).is_err());
    }
}
