//! Catalog lookup: a library asks the branches that joined it whether they
//! can supply a work (`catalog.questions`, `catalog.ask`, `catalog.found`).
//!
//! Work ids are local to a library (a scanned work gets a random id), so a
//! question names a work by its strong identifiers: edition ASINs (with
//! their marketplace), ISBNs and OpenLibrary edition and work ids. The
//! asking library's own work id travels only as a correlation key
//! ([`LookupWork::work`]), so the answer can be matched to the question;
//! the branch never interprets it. Titles and credits are never compared.
//!
//! The library stores the questions for each branch and wakes it; the
//! branch pulls them, because a wake is a hint and not state:
//!
//! 1. branch → library: [`crate::Body::CatalogQuestions`]; answered with
//!    [`crate::Body::CatalogAsk`], which holds at most [`MAX_LOOKUP_WORKS`]
//!    questions (none when nothing is waiting).
//! 2. branch → library: [`crate::Body::CatalogFound`] with one
//!    [`LookupAnswer`] per question it can answer and the rest of the
//!    batch named as missing ("not here"); answered with
//!    [`crate::Body::Accepted`]. A question named in neither stays waiting
//!    and is sent again later.
//!
//! An answer lists the editions the branch can supply, each with its store,
//! product id, formats and bibliographic metadata. Like a
//! [`crate::CatalogTitle`], it carries no account, user or downstream
//! library field, and [`LookupEdition`] refuses unknown fields.

use serde::{Deserialize, Serialize};

use crate::catalog::{is_product_id, is_store_id, CatalogMetadata};

/// Most works one [`crate::Body::CatalogAsk`] or
/// [`crate::Body::CatalogFound`] may name.
pub const MAX_LOOKUP_WORKS: usize = 100;

/// Most identifiers one [`LookupWork`] may carry.
pub const MAX_LOOKUP_IDENTIFIERS: usize = 16;

/// Most editions one [`LookupAnswer`] may list.
pub const MAX_LOOKUP_EDITIONS: usize = 8;

/// Longest correlation key ([`LookupWork::work`]).
pub const MAX_WORK_REF_BYTES: usize = 64;

/// Longest identifier value.
const MAX_IDENTIFIER_BYTES: usize = 64;

/// Longest marketplace.
const MAX_MARKETPLACE_BYTES: usize = 16;

/// Longest metadata text field in an answer.
const MAX_TEXT_BYTES: usize = 2048;

/// Which kind of identifier a [`LookupIdentifier`] is. Only identifiers
/// that name one edition or one work are asked about; anything else does
/// not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookupScheme {
    /// Amazon ASIN of an edition in one marketplace (strong).
    Asin,
    /// ISBN-13 of an edition (weak: one ISBN can sit on several editions).
    Isbn,
    /// OpenLibrary edition id (strong).
    OpenlibraryEdition,
    /// OpenLibrary work id (names the work).
    OpenlibraryWork,
}

impl LookupScheme {
    /// The scheme as the library model stores it (`edition_identifiers.scheme`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asin => "asin",
            Self::Isbn => "isbn",
            Self::OpenlibraryEdition => "openlibrary_edition",
            Self::OpenlibraryWork => "openlibrary_work",
        }
    }

    /// Parses stored scheme text; `None` for any other.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "asin" => Some(Self::Asin),
            "isbn" => Some(Self::Isbn),
            "openlibrary_edition" => Some(Self::OpenlibraryEdition),
            "openlibrary_work" => Some(Self::OpenlibraryWork),
            _ => None,
        }
    }

    /// True for the one scheme scoped to a marketplace (ASIN).
    #[must_use]
    pub fn is_scoped(self) -> bool {
        matches!(self, Self::Asin)
    }
}

/// One identifier of the work a question is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupIdentifier {
    /// The scheme.
    pub scheme: LookupScheme,
    /// The normalised value (an ASIN, an ISBN-13, an OpenLibrary id).
    pub value: String,
    /// Lower-cased marketplace for an ASIN; empty for every other scheme.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub marketplace: String,
}

impl LookupIdentifier {
    /// True when the value is 1 to 64 ASCII letters, digits and `-`, and
    /// the marketplace is set exactly for an ASIN, as up to 16 lowercase
    /// ASCII letters and digits.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let value_ok = !self.value.is_empty()
            && self.value.len() <= MAX_IDENTIFIER_BYTES
            && self
                .value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let market_ok = if self.scheme.is_scoped() {
            !self.marketplace.is_empty()
                && self.marketplace.len() <= MAX_MARKETPLACE_BYTES
                && self
                    .marketplace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        } else {
            self.marketplace.is_empty()
        };
        value_ok && market_ok
    }
}

/// True for a correlation key: 1 to [`MAX_WORK_REF_BYTES`] ASCII letters,
/// digits and `-`.
#[must_use]
pub fn is_work_ref(work: &str) -> bool {
    !work.is_empty()
        && work.len() <= MAX_WORK_REF_BYTES
        && work.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// One question: "can you supply this work?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupWork {
    /// The asking library's work id, echoed in the answer; opaque to the
    /// branch.
    pub work: String,
    /// The work's identifiers; at least one, at most
    /// [`MAX_LOOKUP_IDENTIFIERS`].
    pub identifiers: Vec<LookupIdentifier>,
}

impl LookupWork {
    /// Why the question is malformed, or `None`: a correlation key
    /// ([`is_work_ref`]) and 1 to [`MAX_LOOKUP_IDENTIFIERS`] distinct
    /// well-formed identifiers.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !is_work_ref(&self.work) {
            return Some("invalid_work");
        }
        if self.identifiers.is_empty() || self.identifiers.len() > MAX_LOOKUP_IDENTIFIERS {
            return Some("invalid_identifiers");
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &self.identifiers {
            if !id.is_well_formed() || !seen.insert(id) {
                return Some("invalid_identifiers");
            }
        }
        None
    }
}

/// How a branch can supply an edition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// The branch's own library holds it (its owner's or a member's book).
    Owned,
    /// The branch can borrow it from another library it joined, which
    /// allows onward lending.
    Reachable,
}

/// A file format a branch has for an edition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookupFormat {
    /// The audiobook itself.
    Audio,
    /// The companion PDF.
    Pdf,
}

/// One edition a branch can supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupEdition {
    /// The store that sells it, as the store's plain id (`audible`,
    /// `libro`, ...).
    pub store: String,
    /// Lower-cased marketplace of the store listing (`us`); empty when the
    /// store has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub marketplace: String,
    /// The store's product id (ASIN, ISBN, ...).
    pub product_id: String,
    /// Whether the branch holds it or can borrow it.
    pub availability: Availability,
    /// Formats the branch has; empty when unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<LookupFormat>,
    /// Bibliographic metadata, as the branch's library has it.
    pub metadata: CatalogMetadata,
}

/// True when an optional text field is bounded and has no control
/// characters other than newlines.
fn text_ok(text: Option<&str>) -> bool {
    text.is_none_or(|text| {
        text.len() <= MAX_TEXT_BYTES && !text.chars().any(|c| c.is_control() && c != '\n')
    })
}

impl LookupEdition {
    /// Why the edition is malformed, or `None`: a store id, product id and
    /// marketplace within their rules, distinct formats, a non-empty
    /// bounded title and bounded metadata.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !is_store_id(&self.store) {
            return Some("invalid_store");
        }
        if !is_product_id(&self.product_id) {
            return Some("invalid_product_id");
        }
        if self.marketplace.len() > MAX_MARKETPLACE_BYTES
            || !self
                .marketplace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Some("invalid_marketplace");
        }
        let mut formats = self.formats.clone();
        formats.sort();
        formats.dedup();
        if formats.len() != self.formats.len() {
            return Some("invalid_formats");
        }
        let meta = &self.metadata;
        if meta.title.trim().is_empty() || !text_ok(Some(&meta.title)) {
            return Some("invalid_title");
        }
        let texts = [
            meta.subtitle.as_deref(),
            meta.authors.as_deref(),
            meta.narrators.as_deref(),
            meta.series.as_deref(),
            meta.series_index.as_deref(),
            meta.asin.as_deref(),
            meta.isbn.as_deref(),
            meta.publisher.as_deref(),
            meta.content_kind.as_deref(),
            meta.language.as_deref(),
            meta.published_at.as_deref(),
        ];
        if !texts.into_iter().all(text_ok) {
            return Some("invalid_metadata");
        }
        if meta
            .length_minutes
            .is_some_and(|m| !(0..=1_000_000).contains(&m))
        {
            return Some("invalid_metadata");
        }
        None
    }
}

/// A branch's answer to one [`LookupWork`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupAnswer {
    /// The question's [`LookupWork::work`].
    pub work: String,
    /// What the branch can supply; 1 to [`MAX_LOOKUP_EDITIONS`] editions.
    pub editions: Vec<LookupEdition>,
}

impl LookupAnswer {
    /// Why the answer is malformed, or `None`: a correlation key, and 1 to
    /// [`MAX_LOOKUP_EDITIONS`] well-formed editions with distinct
    /// (store, marketplace, product id).
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !is_work_ref(&self.work) {
            return Some("invalid_work");
        }
        if self.editions.is_empty() || self.editions.len() > MAX_LOOKUP_EDITIONS {
            return Some("invalid_editions");
        }
        let mut seen = std::collections::BTreeSet::new();
        for edition in &self.editions {
            if let Some(problem) = edition.problem() {
                return Some(problem);
            }
            if !seen.insert((
                edition.store.as_str(),
                edition.marketplace.as_str(),
                edition.product_id.as_str(),
            )) {
                return Some("invalid_editions");
            }
        }
        None
    }
}

/// Why a [`crate::Body::CatalogFound`] is malformed, or `None`: at most
/// [`MAX_LOOKUP_WORKS`] works named in all, each once, every answer
/// well-formed and every missing work a correlation key.
#[must_use]
pub fn found_problem(answers: &[LookupAnswer], missing: &[String]) -> Option<&'static str> {
    if answers.len().saturating_add(missing.len()) > MAX_LOOKUP_WORKS {
        return Some("too_many_works");
    }
    let mut works = std::collections::BTreeSet::new();
    for answer in answers {
        if let Some(problem) = answer.problem() {
            return Some(problem);
        }
        if !works.insert(answer.work.as_str()) {
            return Some("duplicate_work");
        }
    }
    for work in missing {
        if !is_work_ref(work) {
            return Some("invalid_work");
        }
        if !works.insert(work.as_str()) {
            return Some("duplicate_work");
        }
    }
    None
}

/// Why a list of questions is malformed, or `None`: at most
/// [`MAX_LOOKUP_WORKS`] well-formed questions, each about a different work.
#[must_use]
pub fn questions_problem(works: &[LookupWork]) -> Option<&'static str> {
    if works.len() > MAX_LOOKUP_WORKS {
        return Some("too_many_works");
    }
    let mut seen = std::collections::BTreeSet::new();
    for work in works {
        if let Some(problem) = work.problem() {
            return Some(problem);
        }
        if !seen.insert(work.work.as_str()) {
            return Some("duplicate_work");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asin(value: &str) -> LookupIdentifier {
        LookupIdentifier {
            scheme: LookupScheme::Asin,
            value: value.into(),
            marketplace: "us".into(),
        }
    }

    fn edition() -> LookupEdition {
        LookupEdition {
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: "B0TEST".into(),
            availability: Availability::Owned,
            formats: vec![LookupFormat::Audio],
            metadata: CatalogMetadata {
                title: "A Title".into(),
                ..CatalogMetadata::default()
            },
        }
    }

    #[test]
    fn questions_name_identifiers_only() {
        let work = LookupWork {
            work: "0b8f-work".into(),
            identifiers: vec![asin("B0TEST")],
        };
        assert_eq!(work.problem(), None);
        let mut value = serde_json::to_value(&work).unwrap();
        value["title"] = serde_json::json!("A Title");
        assert!(serde_json::from_value::<LookupWork>(value).is_err());
        let mut value = serde_json::to_value(&work).unwrap();
        value["identifiers"][0]["scheme"] = serde_json::json!("title");
        assert!(serde_json::from_value::<LookupWork>(value).is_err());
    }

    #[test]
    fn identifiers_are_validated() {
        assert!(asin("B0TEST").is_well_formed());
        let mut unscoped = asin("B0TEST");
        unscoped.marketplace.clear();
        assert!(!unscoped.is_well_formed());
        let isbn = LookupIdentifier {
            scheme: LookupScheme::Isbn,
            value: "9780000000001".into(),
            marketplace: "us".into(),
        };
        assert!(!isbn.is_well_formed());
        assert!(!asin("B0 TEST").is_well_formed());
        let mut work = LookupWork {
            work: "w".into(),
            identifiers: vec![asin("B0TEST"), asin("B0TEST")],
        };
        assert_eq!(work.problem(), Some("invalid_identifiers"));
        work.identifiers.clear();
        assert_eq!(work.problem(), Some("invalid_identifiers"));
        work.identifiers.push(asin("B0TEST"));
        work.work = "w w".into();
        assert_eq!(work.problem(), Some("invalid_work"));
        for scheme in [
            LookupScheme::Asin,
            LookupScheme::Isbn,
            LookupScheme::OpenlibraryEdition,
            LookupScheme::OpenlibraryWork,
        ] {
            assert_eq!(LookupScheme::parse(scheme.as_str()), Some(scheme));
        }
        assert_eq!(LookupScheme::parse("title"), None);
    }

    #[test]
    fn answers_carry_no_account_or_user_field() {
        let answer = LookupAnswer {
            work: "w".into(),
            editions: vec![edition()],
        };
        assert_eq!(answer.problem(), None);
        let text = serde_json::to_string(&answer).unwrap();
        for forbidden in ["account", "user", "token", "node", "cluster", "owner"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
        let mut value = serde_json::to_value(&answer).unwrap();
        value["editions"][0]["account_id"] = serde_json::json!("acct");
        assert!(serde_json::from_value::<LookupAnswer>(value).is_err());
    }

    #[test]
    fn answers_are_validated() {
        let ok = LookupAnswer {
            work: "w".into(),
            editions: vec![edition()],
        };
        let mut bad = ok.clone();
        bad.editions.push(edition());
        assert_eq!(bad.problem(), Some("invalid_editions"));
        let mut bad = ok.clone();
        bad.editions.clear();
        assert_eq!(bad.problem(), Some("invalid_editions"));
        let mut bad = ok.clone();
        bad.editions[0].formats.push(LookupFormat::Audio);
        assert_eq!(bad.problem(), Some("invalid_formats"));
        let mut bad = ok.clone();
        bad.editions[0].marketplace = "US".into();
        assert_eq!(bad.problem(), Some("invalid_marketplace"));
        let mut bad = ok.clone();
        bad.editions[0].metadata.title = " ".into();
        assert_eq!(bad.problem(), Some("invalid_title"));
        assert_eq!(
            found_problem(&[ok.clone(), ok.clone()], &[]),
            Some("duplicate_work")
        );
        assert_eq!(
            found_problem(std::slice::from_ref(&ok), &["w".into()]),
            Some("duplicate_work")
        );
        assert_eq!(
            found_problem(std::slice::from_ref(&ok), &["x y".into()]),
            Some("invalid_work")
        );
        assert_eq!(found_problem(&[ok], &["v".into()]), None);
    }
}
