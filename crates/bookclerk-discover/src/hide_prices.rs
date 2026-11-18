//! Taking prices and store links out of Discover answers, for a patron
//! whose member controls turn "See prices and store links" off.
//!
//! Each answer keeps which stores carry a title (the store and its
//! product id), so cards still group and match, but loses every price and
//! every link into a store.

use crate::catalog_search::{CatalogSearchHit, CatalogSearchPage};
use crate::purchase::{PurchaseHint, PurchaseHintsResponse};
use crate::recommend::Recommendation;
use crate::shelves::DiscoverFeed;

impl PurchaseHint {
    /// Removes the hint's store link and every price it carries.
    pub fn hide_price_and_link(&mut self) {
        self.url = None;
        self.price_cents = None;
        self.currency = None;
        self.price_label = None;
        self.list_price_cents = None;
        self.list_price_label = None;
        self.member_price_cents = None;
        self.member_price_label = None;
    }
}

impl PurchaseHintsResponse {
    /// Removes the store links and prices of every hint, `best` included.
    pub fn hide_prices_and_links(&mut self) {
        for hint in self.hints.iter_mut().chain(self.best.iter_mut()) {
            hint.hide_price_and_link();
        }
    }
}

impl Recommendation {
    /// Removes the card's store links and prices.
    pub fn hide_prices_and_links(&mut self) {
        for hint in &mut self.purchase_hints {
            hint.hide_price_and_link();
        }
    }
}

impl DiscoverFeed {
    /// Removes the store links and prices of every card on every shelf.
    pub fn hide_prices_and_links(&mut self) {
        for item in self
            .shelves
            .iter_mut()
            .flat_map(|shelf| shelf.items.iter_mut())
        {
            item.hide_prices_and_links();
        }
    }
}

impl CatalogSearchHit {
    /// Removes the hit's price and its store links and prices.
    pub fn hide_prices_and_links(&mut self) {
        self.price_cents = None;
        for hint in &mut self.purchase_hints {
            hint.hide_price_and_link();
        }
    }
}

impl CatalogSearchPage {
    /// Removes the prices and store links of every hit.
    pub fn hide_prices_and_links(&mut self) {
        for item in &mut self.items {
            item.hide_prices_and_links();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(source: &str) -> PurchaseHint {
        PurchaseHint {
            source: source.into(),
            product_id: "9780441478125".into(),
            title: Some("The Left Hand of Darkness".into()),
            url: Some("https://libro.fm/audiobooks/9780441478125".into()),
            price_cents: Some(1499),
            currency: Some("USD".into()),
            price_label: Some("$14.99".into()),
            list_price_cents: Some(2999),
            list_price_label: Some("$29.99".into()),
            member_price_cents: Some(1499),
            member_price_label: Some("$14.99".into()),
        }
    }

    fn bare(source: &str) -> PurchaseHint {
        PurchaseHint {
            source: source.into(),
            product_id: "9780441478125".into(),
            title: Some("The Left Hand of Darkness".into()),
            ..PurchaseHint::default()
        }
    }

    #[test]
    fn a_hint_keeps_its_store_and_product_and_loses_the_rest() {
        let mut hint = priced("libro");
        hint.hide_price_and_link();
        assert_eq!(hint, bare("libro"));
    }

    #[test]
    fn every_hint_of_an_answer_loses_its_price_and_link() {
        let mut response = PurchaseHintsResponse {
            hints: vec![priced("libro"), priced("audible")],
            best: Some(priced("libro")),
            incomplete: false,
        };
        response.hide_prices_and_links();
        assert_eq!(response.hints, vec![bare("libro"), bare("audible")]);
        assert_eq!(response.best, Some(bare("libro")));
    }

    #[test]
    fn a_search_hit_loses_its_price() {
        let mut hit: CatalogSearchHit = serde_json::from_value(serde_json::json!({
            "work_key": "isbn:9780441478125",
            "title": "The Left Hand of Darkness",
            "price_cents": 1499,
            "store_editions": [],
            "sources": ["libro"],
        }))
        .unwrap_or_else(|err| panic!("hit: {err}"));
        hit.purchase_hints = vec![priced("libro")];
        hit.hide_prices_and_links();
        assert_eq!(hit.price_cents, None);
        assert_eq!(hit.purchase_hints, vec![bare("libro")]);
    }
}
