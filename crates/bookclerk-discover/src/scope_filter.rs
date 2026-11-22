//! Keeping Discover answers inside a patron's scope.
//!
//! Shelves and storefront search show titles the library may not hold, so
//! the page index cannot filter them. Each card's own metadata (authors,
//! narrators, series, genres and the explicit flag) is checked against the
//! patron's scope instead, with [`WorkScope::admits_unheld`].

use bookclerk_library::{UnheldItem, WorkScope};

use crate::catalog_search::{CatalogSearchHit, CatalogSearchPage};
use crate::recommend::Recommendation;
use crate::shelves::DiscoverFeed;

impl Recommendation {
    /// The card's metadata as a scope reads it.
    #[must_use]
    pub fn unheld_item(&self) -> UnheldItem<'_> {
        UnheldItem {
            authors: self.authors.as_deref(),
            narrators: self.narrators.as_deref(),
            series: self.series.as_deref(),
            genres: self.genres.as_deref(),
            is_explicit: self.is_explicit,
        }
    }
}

impl CatalogSearchHit {
    /// The hit's metadata as a scope reads it.
    #[must_use]
    pub fn unheld_item(&self) -> UnheldItem<'_> {
        UnheldItem {
            authors: self.authors.as_deref(),
            narrators: self.narrators.as_deref(),
            series: self.series.as_deref(),
            genres: self.genres.as_deref(),
            is_explicit: self.is_explicit,
        }
    }
}

impl DiscoverFeed {
    /// Drops every card outside `scope`, then every shelf left empty.
    pub fn retain_in_scope(&mut self, scope: &WorkScope) {
        for shelf in &mut self.shelves {
            shelf
                .items
                .retain(|item| scope.admits_unheld(&item.unheld_item()));
        }
        self.shelves.retain(|shelf| !shelf.items.is_empty());
    }
}

impl CatalogSearchPage {
    /// Drops every hit outside `scope`. The page keeps its cursor, so it
    /// can come back shorter than its size while more pages follow.
    pub fn retain_in_scope(&mut self, scope: &WorkScope) {
        self.items
            .retain(|hit| scope.admits_unheld(&hit.unheld_item()));
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_library::WorkFacet;

    use super::*;
    use crate::shelves::DiscoverShelf;

    fn card(title: &str, genres: &str, explicit: Option<bool>) -> Recommendation {
        Recommendation {
            title: title.into(),
            authors: Some("Ann Writer, Bo Pen".into()),
            genres: Some(genres.into()),
            is_explicit: explicit,
            ..Recommendation::default()
        }
    }

    fn feed() -> DiscoverFeed {
        DiscoverFeed {
            shelves: vec![
                DiscoverShelf {
                    id: "picks".into(),
                    title: "Picks".into(),
                    subtitle: None,
                    items: vec![
                        card("Kids book", "Children; Adventure", Some(false)),
                        card("Thriller", "Mystery", None),
                        card("Adult", "Children", Some(true)),
                    ],
                },
                DiscoverShelf {
                    id: "mystery".into(),
                    title: "Mystery".into(),
                    subtitle: None,
                    items: vec![card("Thriller 2", "Mystery", None)],
                },
            ],
            ..DiscoverFeed::default()
        }
    }

    fn titles(feed: &DiscoverFeed) -> Vec<Vec<&str>> {
        feed.shelves
            .iter()
            .map(|s| s.items.iter().map(|i| i.title.as_str()).collect())
            .collect()
    }

    #[test]
    fn an_include_by_genre_keeps_matching_cards_and_drops_empty_shelves() {
        let scope = WorkScope {
            include: vec![(WorkFacet::Genre, bookclerk_library::facet_key("children"))],
            exclude: vec![(WorkFacet::Explicit, "explicit".into())],
            owner: None,
        };
        let mut feed = feed();
        feed.retain_in_scope(&scope);
        assert_eq!(titles(&feed), vec![vec!["Kids book"]]);
    }

    #[test]
    fn an_exclude_by_author_hides_a_co_written_card() {
        let scope = WorkScope {
            include: Vec::new(),
            exclude: vec![(WorkFacet::Author, bookclerk_library::facet_key("Bo Pen"))],
            owner: None,
        };
        let mut feed = feed();
        feed.retain_in_scope(&scope);
        assert!(feed.shelves.is_empty());
    }

    #[test]
    fn an_include_only_by_tag_admits_no_catalog_card() {
        let scope = WorkScope {
            include: vec![(WorkFacet::Tag, "kids".into())],
            exclude: Vec::new(),
            owner: None,
        };
        let mut page = CatalogSearchPage {
            items: vec![
                serde_json::from_value::<CatalogSearchHit>(serde_json::json!({
                    "work_key": "k",
                    "title": "Any",
                    "store_editions": [],
                    "sources": [],
                }))
                .expect("a minimal hit"),
            ],
            page_size: 1,
            has_more: true,
            next_cursor: Some("c".into()),
            sort: "relevance".into(),
            sort_dir: "desc".into(),
        };
        page.retain_in_scope(&scope);
        assert!(page.items.is_empty());
        assert_eq!(page.next_cursor.as_deref(), Some("c"), "paging goes on");
    }
}
