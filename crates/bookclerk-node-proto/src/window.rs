//! The person's view of a library's books (`window.books`, `window.page`):
//! a branch reads every book of each library it joined, as the library's
//! own Library page lists them for its person, so it can show them beside
//! the books of every other library it joined. A clerk reads the same
//! pages over its clerk socket.
//!
//! The person is the branch node's owner on the library, so the request
//! names no user or account. The library answers with every book on its
//! store accounts, and marks each edition the person holds through a store
//! account of their own ([`WindowEdition::yours`]).
//!
//! 1. window → library: [`crate::Body::WindowBooks`] with the cursor of the
//!    previous page ([`WindowCursor`]; empty for the first).
//! 2. library → window: [`crate::Body::WindowPage`] with at most
//!    [`MAX_WINDOW_WORKS`] works and the cursor of the next page, or none
//!    when this was the last page.
//!
//! Work ids are local to each library, so a [`WindowWork`] carries the
//! library's own work id only as a grouping key ([`WindowWork::work`]);
//! the window merges works from several libraries by their identifiers
//! (ASIN with marketplace, ISBN, OpenLibrary ids), never by title. Like a
//! catalog lookup answer, a page carries no account, user or downstream
//! library field, and every type refuses unknown fields.

use serde::{Deserialize, Serialize};

use crate::lookup::{
    is_work_ref, Availability, LookupEdition, LookupIdentifier, MAX_LOOKUP_EDITIONS,
    MAX_LOOKUP_IDENTIFIERS,
};

/// Most works one [`crate::Body::WindowPage`] may carry.
pub const MAX_WINDOW_WORKS: usize = 100;

/// Where a page of the person's books starts: an opaque key the library
/// handed out as the previous page's `next`, or empty for the first page.
///
/// A cursor is empty or, like a grouping key ([`is_work_ref`]), at most
/// [`crate::lookup::MAX_WORK_REF_BYTES`] bytes of ASCII letters, digits and `-`. Cursors
/// order as strings, and each page's `next` sorts after the cursor it
/// answers, so paging always moves forward. On the wire it is a JSON
/// string; anything else is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WindowCursor(String);

impl WindowCursor {
    /// The cursor of the first page.
    #[must_use]
    pub fn start() -> Self {
        Self(String::new())
    }

    /// Reads a cursor: `None` unless `text` is empty or a well-formed key.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        (text.is_empty() || is_work_ref(text)).then(|| Self(text.to_owned()))
    }

    /// True for the first page's cursor.
    #[must_use]
    pub fn is_start(&self) -> bool {
        self.0.is_empty()
    }

    /// The cursor's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WindowCursor {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.is_empty() || is_work_ref(&text) {
            Ok(Self(text))
        } else {
            Err("invalid_cursor")
        }
    }
}

impl From<WindowCursor> for String {
    fn from(cursor: WindowCursor) -> Self {
        cursor.0
    }
}

impl std::fmt::Display for WindowCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One work the library holds for the window's person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowWork {
    /// The library's own work id (or another key unique within the page)
    /// that groups these editions; opaque to the window and never compared
    /// across libraries.
    pub work: String,
    /// The work's identifiers in the library, strong ones first; at most
    /// [`MAX_LOOKUP_IDENTIFIERS`], and empty when the library has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identifiers: Vec<LookupIdentifier>,
    /// The library's editions of the work; 1 to [`MAX_LOOKUP_EDITIONS`],
    /// each [`Availability::Owned`].
    pub editions: Vec<WindowEdition>,
}

/// One edition of a [`WindowWork`]: the edition as the library holds it,
/// and whether it is the person's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowEdition {
    /// The edition, with every format any of the library's copies has.
    pub edition: LookupEdition,
    /// True when the person holds it through a store account of their own
    /// on the library; false for a book only other people there hold.
    /// A hint for the reader: anything the library hands out on the
    /// strength of it is checked again there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub yours: bool,
}

impl WindowWork {
    /// Why the work is malformed, or `None`: a grouping key
    /// ([`is_work_ref`]), at most [`MAX_LOOKUP_IDENTIFIERS`] distinct
    /// well-formed identifiers, and 1 to [`MAX_LOOKUP_EDITIONS`] well-formed
    /// editions the library holds, with distinct (store, marketplace,
    /// product id).
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !is_work_ref(&self.work) {
            return Some("invalid_work");
        }
        if self.identifiers.len() > MAX_LOOKUP_IDENTIFIERS {
            return Some("invalid_identifiers");
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &self.identifiers {
            if !id.is_well_formed() || !seen.insert(id) {
                return Some("invalid_identifiers");
            }
        }
        if self.editions.is_empty() || self.editions.len() > MAX_LOOKUP_EDITIONS {
            return Some("invalid_editions");
        }
        let mut keys = std::collections::BTreeSet::new();
        for edition in self.editions.iter().map(|e| &e.edition) {
            if let Some(problem) = edition.problem() {
                return Some(problem);
            }
            if edition.availability != Availability::Owned {
                return Some("invalid_availability");
            }
            if !keys.insert((
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

/// Why a [`crate::Body::WindowPage`] is malformed, or `None`: at most
/// [`MAX_WINDOW_WORKS`] well-formed works with distinct grouping keys, and a
/// next cursor, when there is one, past `after` (the cursor the window
/// asked with), so paging always moves forward.
#[must_use]
pub fn window_page_problem(
    after: &WindowCursor,
    works: &[WindowWork],
    next: Option<&WindowCursor>,
) -> Option<&'static str> {
    if works.len() > MAX_WINDOW_WORKS {
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
    if next.is_some_and(|next| next <= after) {
        return Some("invalid_cursor");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogMetadata;
    use crate::lookup::MAX_WORK_REF_BYTES;
    use crate::lookup::{LookupFormat, LookupScheme};

    fn edition(product_id: &str) -> LookupEdition {
        LookupEdition {
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: product_id.into(),
            availability: Availability::Owned,
            formats: vec![LookupFormat::Audio],
            metadata: CatalogMetadata {
                title: "A Title".into(),
                ..CatalogMetadata::default()
            },
        }
    }

    fn work() -> WindowWork {
        WindowWork {
            work: "0b8f-work".into(),
            identifiers: vec![LookupIdentifier {
                scheme: LookupScheme::Asin,
                value: "B0TEST".into(),
                marketplace: "us".into(),
            }],
            editions: vec![WindowEdition {
                edition: edition("B0TEST"),
                yours: true,
            }],
        }
    }

    #[test]
    fn a_page_names_no_account_user_or_library() {
        let work = work();
        assert_eq!(work.problem(), None);
        let text = serde_json::to_string(&work).unwrap();
        for forbidden in ["account", "user", "token", "node", "cluster", "owner"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
        let mut value = serde_json::to_value(&work).unwrap();
        value["account_id"] = serde_json::json!("acct");
        assert!(serde_json::from_value::<WindowWork>(value).is_err());
        let mut value = serde_json::to_value(&work).unwrap();
        value["editions"][0]["user"] = serde_json::json!(1);
        assert!(serde_json::from_value::<WindowWork>(value).is_err());
        let mut value = serde_json::to_value(&work).unwrap();
        value["editions"][0]["edition"]["user"] = serde_json::json!(1);
        assert!(serde_json::from_value::<WindowWork>(value).is_err());
    }

    #[test]
    fn yours_is_sent_only_when_true() {
        let mut work = work();
        let text = serde_json::to_string(&work).unwrap();
        assert!(text.contains("\"yours\":true"), "{text}");
        work.editions[0].yours = false;
        let text = serde_json::to_string(&work).unwrap();
        assert!(!text.contains("yours"), "{text}");
        let back: WindowWork = serde_json::from_str(&text).unwrap();
        assert!(
            !back.editions[0].yours,
            "an edition without the flag is not yours"
        );
    }

    #[test]
    fn works_are_validated() {
        let mut bad = work();
        bad.work = "a b".into();
        assert_eq!(bad.problem(), Some("invalid_work"));
        let mut bad = work();
        bad.editions.clear();
        assert_eq!(bad.problem(), Some("invalid_editions"));
        let mut bad = work();
        bad.editions.push(WindowEdition {
            edition: edition("B0TEST"),
            yours: false,
        });
        assert_eq!(bad.problem(), Some("invalid_editions"));
        let mut bad = work();
        bad.editions[0].edition.availability = Availability::Reachable;
        assert_eq!(bad.problem(), Some("invalid_availability"));
        let mut bad = work();
        bad.identifiers.push(bad.identifiers[0].clone());
        assert_eq!(bad.problem(), Some("invalid_identifiers"));
        let mut bad = work();
        bad.identifiers[0].marketplace.clear();
        assert_eq!(bad.problem(), Some("invalid_identifiers"));
        let mut none = work();
        none.identifiers.clear();
        assert_eq!(
            none.problem(),
            None,
            "a work with no identifiers is still shown"
        );
    }

    #[test]
    fn pages_are_validated() {
        let ok = work();
        let start = WindowCursor::start();
        let at = |text: &str| WindowCursor::parse(text).unwrap();
        assert_eq!(
            window_page_problem(&start, std::slice::from_ref(&ok), Some(&at("5"))),
            None
        );
        assert_eq!(window_page_problem(&start, &[], None), None);
        assert_eq!(
            window_page_problem(&start, &[ok.clone(), ok.clone()], None),
            Some("duplicate_work")
        );
        for next in ["5", "4", ""] {
            assert_eq!(
                window_page_problem(&at("5"), std::slice::from_ref(&ok), Some(&at(next))),
                Some("invalid_cursor"),
                "{next:?}"
            );
        }
        let many: Vec<WindowWork> = (0..=MAX_WINDOW_WORKS)
            .map(|i| WindowWork {
                work: format!("w{i}"),
                ..work()
            })
            .collect();
        assert_eq!(
            window_page_problem(&start, &many, None),
            Some("too_many_works")
        );
    }

    #[test]
    fn cursors_are_empty_or_keys() {
        assert!(WindowCursor::start().is_start());
        assert_eq!(WindowCursor::parse(""), Some(WindowCursor::start()));
        let id = "0b5c2e6a-9f1d-4c3b-8a7e-2d4f6b8c0e1a";
        assert_eq!(
            WindowCursor::parse(id).map(|c| c.to_string()),
            Some(id.into())
        );
        for bad in ["a b", "x/y", "é", &"a".repeat(MAX_WORK_REF_BYTES + 1)] {
            assert_eq!(WindowCursor::parse(bad), None, "{bad:?}");
        }
        let json: WindowCursor = serde_json::from_str(&format!("\"{id}\"")).unwrap();
        assert_eq!(json.as_str(), id);
        assert_eq!(serde_json::to_string(&json).unwrap(), format!("\"{id}\""));
        assert!(serde_json::from_str::<WindowCursor>("5").is_err());
        assert!(serde_json::from_str::<WindowCursor>("\"a b\"").is_err());
    }
}
