//! "Available through a branch": the branches of this library that answered
//! a catalog lookup for a work, shown on Discover cards and search hits.
//!
//! Branch answers are `branch:<node>` listings in the shared listing pool
//! (see `bookclerk_library::control_plane::branch_offers_for_works`). They
//! hang off an edition the library already has, so a card or hit is matched
//! to them through its work: the card's own work id, or the work of a store
//! listing the pool holds for one of its store editions. Titles are never
//! compared.
//!
//! The field is filled when a feed or a search page is read, never stored
//! with the ranked feed, so a branch that stops listing a book drops off
//! every card on the next read.

use std::collections::{BTreeSet, HashMap};

use bookclerk_library::control_plane::{branch_offers_for_works, works_of_listings, BranchOffer};
use bookclerk_library::LibraryStore;

use crate::catalog_search::CatalogSearchHit;
use crate::error::Result;
use crate::identity::StoreEdition;
use crate::shelves::DiscoverFeed;
use crate::stored_feed::CandidatePool;

/// One branch a title is available through.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BranchAvailability {
    /// The branch node id.
    #[schema(example = "node-7f3a")]
    pub node_id: String,
    /// The branch owner's display name, for "available through Riley's
    /// branch"; absent when the owner has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Riley")]
    pub owner_name: Option<String>,
    /// The store the branch has the book from.
    #[schema(example = "audible")]
    pub store: String,
    /// The store's product id of the branch's copy.
    #[schema(example = "B002V0QK4C")]
    pub product_id: String,
}

impl From<BranchOffer> for BranchAvailability {
    fn from(offer: BranchOffer) -> Self {
        Self {
            node_id: offer.node_id,
            owner_name: offer.owner_name,
            store: offer.store,
            product_id: offer.product_id,
        }
    }
}

/// The availability of each work in `offers`, converted for the wire.
fn by_work(offers: HashMap<String, Vec<BranchOffer>>) -> HashMap<String, Vec<BranchAvailability>> {
    offers
        .into_iter()
        .map(|(work, list)| (work, list.into_iter().map(Into::into).collect()))
        .collect()
}

/// Sets `available_through` on every card of `feed`.
///
/// A card's work is its `work_id`, or else the work of the first of its
/// store editions the candidate pool holds. Cards with no work, or whose
/// work no branch currently lists, get an empty list.
///
/// # Arguments
///
/// * `library` - The library whose branch answers are read.
/// * `feed` - The feed as read for one subject.
/// * `pool` - The subject's candidate pool, when it was read.
///
/// # Errors
///
/// Returns the library read error.
pub async fn join_branch_availability(
    library: &LibraryStore,
    feed: &mut DiscoverFeed,
    pool: Option<&CandidatePool>,
) -> Result<()> {
    let mut work_of_card: Vec<Option<String>> = Vec::new();
    for card in feed.shelves.iter().flat_map(|s| &s.items) {
        let work = card
            .work_id
            .clone()
            .filter(|w| !w.is_empty())
            .or_else(|| pool.and_then(|p| p.work_for(card)).map(str::to_owned));
        work_of_card.push(work);
    }
    let works: Vec<String> = work_of_card
        .iter()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let available = by_work(branch_offers_for_works(library, &works).await?);
    for (card, work) in feed
        .shelves
        .iter_mut()
        .flat_map(|s| s.items.iter_mut())
        .zip(work_of_card)
    {
        card.available_through = work
            .and_then(|w| available.get(&w).cloned())
            .unwrap_or_default();
    }
    Ok(())
}

/// Sets `available_through` on every hit of a catalog search page.
///
/// A hit's work is the work of the first of its store editions the shared
/// listing pool holds (an owned, wished or recommended book's listing). A
/// hit the pool knows nothing about gets an empty list.
///
/// # Arguments
///
/// * `library` - The library whose pool and branch answers are read.
/// * `hits` - The page's hits.
/// * `plugin_key_of` - The PluginKey a storefront id's listings are pooled
///   under, or `None` for a source that is not deployed.
///
/// # Errors
///
/// Returns the library read error.
pub async fn join_search_branch_availability(
    library: &LibraryStore,
    hits: &mut [CatalogSearchHit],
    plugin_key_of: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    let keyed = |edition: &StoreEdition| {
        plugin_key_of(&edition.source).map(|key| (key, edition.product_id.clone()))
    };
    let listings: Vec<(String, String)> = hits
        .iter()
        .flat_map(|h| h.store_editions.iter().filter_map(keyed))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if listings.is_empty() {
        return Ok(());
    }
    let work_of_listing = works_of_listings(library, &listings).await?;
    let work_of_hit: Vec<Option<String>> = hits
        .iter()
        .map(|h| {
            h.store_editions
                .iter()
                .filter_map(keyed)
                .find_map(|key| work_of_listing.get(&key).cloned())
        })
        .collect();
    let works: Vec<String> = work_of_hit
        .iter()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let available = by_work(branch_offers_for_works(library, &works).await?);
    for (hit, work) in hits.iter_mut().zip(work_of_hit) {
        hit.available_through = work
            .and_then(|w| available.get(&w).cloned())
            .unwrap_or_default();
    }
    Ok(())
}
