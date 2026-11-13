//! Window mode: an install whose link has "Use this computer to download"
//! turned off takes no work from that library, and instead reads the books
//! the library holds for its person (`window.books`, `window.page`), so it
//! can show them beside the books of every other library it joined.
//!
//! The person is the branch node's owner on the library, so the request
//! names no user or account; the library answers from the books on the
//! store accounts linked to that person.
//!
//! 1. window → library: [`crate::Body::WindowBooks`] with the cursor of the
//!    previous page (0 for the first).
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
    /// The editions the person holds there; 1 to [`MAX_LOOKUP_EDITIONS`],
    /// each [`Availability::Owned`].
    pub editions: Vec<LookupEdition>,
}

impl WindowWork {
    /// Why the work is malformed, or `None`: a grouping key
    /// ([`is_work_ref`]), at most [`MAX_LOOKUP_IDENTIFIERS`] distinct
    /// well-formed identifiers, and 1 to [`MAX_LOOKUP_EDITIONS`] well-formed
    /// owned editions with distinct (store, marketplace, product id).
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
        for edition in &self.editions {
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
    after: i64,
    works: &[WindowWork],
    next: Option<i64>,
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
            editions: vec![edition("B0TEST")],
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
        bad.editions.push(edition("B0TEST"));
        assert_eq!(bad.problem(), Some("invalid_editions"));
        let mut bad = work();
        bad.editions[0].availability = Availability::Reachable;
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
        assert_eq!(
            window_page_problem(0, std::slice::from_ref(&ok), Some(5)),
            None
        );
        assert_eq!(window_page_problem(0, &[], None), None);
        assert_eq!(
            window_page_problem(0, &[ok.clone(), ok.clone()], None),
            Some("duplicate_work")
        );
        assert_eq!(
            window_page_problem(5, std::slice::from_ref(&ok), Some(5)),
            Some("invalid_cursor")
        );
        let many: Vec<WindowWork> = (0..=MAX_WINDOW_WORKS)
            .map(|i| WindowWork {
                work: format!("w{i}"),
                ..work()
            })
            .collect();
        assert_eq!(window_page_problem(0, &many, None), Some("too_many_works"));
    }
}
