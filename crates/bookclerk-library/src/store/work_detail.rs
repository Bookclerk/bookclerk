//! One work as its detail view shows it: the work, its editions grouped
//! with their expression, each edition's store listings and the holdings
//! of those listings.
//!
//! A podcast show's detail includes its episodes' editions, each naming
//! the episode work it belongs to. Every stored enum is parsed into its
//! typed value; a row whose stored text names no known value is skipped
//! rather than shown wrong.

use std::collections::HashMap;

use bookclerk_resolve::{Abridgement, Entitlement, ExpressionKind, MediaKind, WorkKind};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::library_model::{find_chunked, find_chunked_paged};
use super::LibraryStore;
use crate::entities::{
    edition_acquisitions, editions, expressions, holdings, store_listings, user_title_state, works,
};
use crate::error::{LibraryError, Result};
use crate::models::{AcquireStatus, UserTitleState};

/// Who holds a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldingHolder {
    /// A store account, by plugin and account id.
    Account {
        /// The account's plugin.
        plugin_key: String,
        /// The account id at that plugin.
        account_id: String,
    },
    /// A branch library's node, by node id.
    Node {
        /// The node id.
        node_id: String,
    },
}

/// One holding of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingHolding {
    /// Holding id.
    pub id: String,
    /// Who holds the listing.
    pub holder: HoldingHolder,
    /// On what terms the holder has it.
    pub entitlement: Entitlement,
    /// RFC 3339 purchase time, when the store reported one.
    pub purchased_at: Option<String>,
    /// RFC 3339 time the holding was first recorded.
    pub added_at: String,
}

/// One store listing of an edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionListing {
    /// Listing id.
    pub id: String,
    /// The storefront plugin that lists it.
    pub plugin_key: String,
    /// Store marketplace (`us`, `uk`, …), empty when the store has one.
    pub marketplace: String,
    /// The store's product id (ASIN, ISBN, product UUID, …).
    pub product_id: String,
    /// The product page, when known.
    pub url: Option<String>,
    /// The listing's holdings, by id.
    pub holdings: Vec<ListingHolding>,
}

/// One edition of a work with its expression and listings.
#[derive(Debug, Clone, PartialEq)]
pub struct EditionDetail {
    /// Edition id.
    pub id: String,
    /// The work the edition realises: the work itself, or one of a show's
    /// episodes.
    pub work_id: String,
    /// The expression the edition carries.
    pub expression_id: String,
    /// How the expression realises the work.
    pub expression_kind: ExpressionKind,
    /// The expression's language tag, when known.
    pub language: Option<String>,
    /// Whether the expression is abridged.
    pub abridgement: Abridgement,
    /// Carrier of the edition.
    pub media_kind: MediaKind,
    /// The edition's title.
    pub title: String,
    /// The edition's subtitle.
    pub subtitle: Option<String>,
    /// Publication date as the store gave it.
    pub published_at: Option<String>,
    /// Running time in minutes, for audio.
    pub duration_minutes: Option<i64>,
    /// Cover image URL from the store, when known.
    pub cover_url: Option<String>,
    /// The edition's acquire state.
    pub acquire_status: AcquireStatus,
    /// The edition's listings, by id.
    pub listings: Vec<EditionListing>,
}

/// A top-level work with everything its detail view shows.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkDetail {
    /// Work id.
    pub id: String,
    /// Book or podcast.
    pub kind: WorkKind,
    /// Display title.
    pub title: String,
    /// Blurb or synopsis (may contain HTML).
    pub description: Option<String>,
    /// Authors (hosts for a podcast), in billing order.
    pub authors: Vec<String>,
    /// First series name.
    pub series: Option<String>,
    /// Position in [`Self::series`].
    pub series_position: Option<String>,
    /// Editions, by id; a show's episodes' editions included.
    pub editions: Vec<EditionDetail>,
}

/// Reads a stored holding; `None` when its holder kind or entitlement is
/// not one this binary knows, or a holder column it needs is empty.
fn listing_holding(model: holdings::Model) -> Option<ListingHolding> {
    let holder = match model.holder_kind.as_str() {
        "account" => HoldingHolder::Account {
            plugin_key: model.plugin_key?,
            account_id: model.account_id?,
        },
        "node" => HoldingHolder::Node {
            node_id: model.node_id?,
        },
        _ => return None,
    };
    Some(ListingHolding {
        holder,
        entitlement: Entitlement::parse(&model.entitlement)?,
        id: model.id,
        purchased_at: model.purchased_at,
        added_at: model.created_at,
    })
}

/// An edition's acquire state from its stored row: never acquired without
/// one, `None` (the edition is skipped) for text that names no state.
fn edition_status(stored: Option<&String>) -> Option<AcquireStatus> {
    match stored {
        None => Some(AcquireStatus::NotAcquired),
        Some(text) => AcquireStatus::parse(text),
    }
}

impl LibraryStore {
    /// One top-level work with its editions, listings and holdings.
    ///
    /// # Arguments
    ///
    /// * `work_id` - The work. A podcast episode's id reads as missing:
    ///   its show is the work the library lists.
    ///
    /// # Returns
    ///
    /// `None` when there is no such top-level work.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn library_work_detail(&self, work_id: &str) -> Result<Option<WorkDetail>> {
        let Some(work) = works::Entity::find_by_id(work_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        if work.parent_work_id.is_some() {
            return Ok(None);
        }
        let Some(kind) = WorkKind::parse(&work.kind) else {
            return Ok(None);
        };
        let mut work_ids = vec![work.id.clone()];
        let episodes = find_chunked_paged::<works::Entity, _>(self, &work_ids, |part| {
            works::Entity::find()
                .filter(works::Column::ParentWorkId.is_in(part.iter().cloned()))
                .order_by_asc(works::Column::Id)
        })
        .await?;
        work_ids.extend(episodes.into_iter().map(|w| w.id));
        let expressions: HashMap<String, expressions::Model> =
            find_chunked_paged::<expressions::Entity, _>(self, &work_ids, |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(expressions::Column::Id)
            })
            .await?
            .into_iter()
            .map(|x| (x.id.clone(), x))
            .collect();
        let expression_ids: Vec<String> = expressions.keys().cloned().collect();
        let edition_rows =
            find_chunked_paged::<editions::Entity, _>(self, &expression_ids, |part| {
                editions::Entity::find()
                    .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
                    .order_by_asc(editions::Column::Id)
            })
            .await?;
        let edition_ids: Vec<String> = edition_rows.iter().map(|e| e.id.clone()).collect();
        let listings =
            find_chunked_paged::<store_listings::Entity, _>(self, &edition_ids, |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.iter().cloned()))
                    .order_by_asc(store_listings::Column::Id)
            })
            .await?;
        let listing_ids: Vec<String> = listings.iter().map(|l| l.id.clone()).collect();
        let mut holdings_by_listing: HashMap<String, Vec<ListingHolding>> = HashMap::new();
        for holding in find_chunked_paged::<holdings::Entity, _>(self, &listing_ids, |part| {
            holdings::Entity::find()
                .filter(holdings::Column::ListingId.is_in(part.iter().cloned()))
                .order_by_asc(holdings::Column::Id)
        })
        .await?
        {
            let listing_id = holding.listing_id.clone();
            if let Some(detail) = listing_holding(holding) {
                holdings_by_listing
                    .entry(listing_id)
                    .or_default()
                    .push(detail);
            }
        }
        let mut listings_by_edition: HashMap<String, Vec<EditionListing>> = HashMap::new();
        for listing in listings {
            let holdings = holdings_by_listing.remove(&listing.id).unwrap_or_default();
            listings_by_edition
                .entry(listing.edition_id)
                .or_default()
                .push(EditionListing {
                    id: listing.id,
                    plugin_key: listing.plugin_key,
                    marketplace: listing.marketplace,
                    product_id: listing.product_id,
                    url: listing.url,
                    holdings,
                });
        }
        // Stored text, so an unknown status (skipped) stays apart from no
        // acquire row at all (never acquired).
        let statuses: HashMap<String, String> =
            find_chunked::<edition_acquisitions::Entity, _>(self, &edition_ids, |part| {
                edition_acquisitions::Entity::find()
                    .filter(edition_acquisitions::Column::EditionId.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .map(|a| (a.edition_id, a.status))
            .collect();
        let (authors, _) = self.work_authors(&work, &edition_rows).await?;
        let series = self.work_series(&work.id).await?;
        let editions = edition_rows
            .into_iter()
            .filter_map(|edition| {
                let expression = expressions.get(&edition.expression_id)?;
                Some(EditionDetail {
                    work_id: expression.work_id.clone(),
                    expression_kind: ExpressionKind::parse(&expression.kind)?,
                    language: expression.language.clone(),
                    abridgement: Abridgement::parse(&expression.abridgement)?,
                    media_kind: MediaKind::parse(&edition.media_kind)?,
                    acquire_status: edition_status(statuses.get(&edition.id))?,
                    listings: listings_by_edition.remove(&edition.id).unwrap_or_default(),
                    expression_id: edition.expression_id,
                    title: edition.title,
                    subtitle: edition.subtitle,
                    published_at: edition.published_at,
                    duration_minutes: edition.duration_minutes,
                    cover_url: edition.cover_url,
                    id: edition.id,
                })
            })
            .collect();
        let (series, series_position) = match series {
            Some((name, position)) => (Some(name), position),
            None => (None, None),
        };
        Ok(Some(WorkDetail {
            id: work.id,
            kind,
            title: work.title,
            description: work.description,
            authors,
            series,
            series_position,
            editions,
        }))
    }

    /// The store cover URLs of `edition_ids`, for a page of index rows
    /// (each names its cover edition).
    ///
    /// # Returns
    ///
    /// Edition id to cover URL; editions with no cover are left out.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn edition_cover_urls(
        &self,
        edition_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        Ok(
            find_chunked::<editions::Entity, _>(self, edition_ids, |part| {
                editions::Entity::find().filter(editions::Column::Id.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .filter_map(|e| Some((e.id, e.cover_url.filter(|url| !url.is_empty())?)))
            .collect(),
        )
    }

    /// One user's own state for one work.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user (`users.id`).
    /// * `work_id` - The work (`works.id`).
    ///
    /// # Returns
    ///
    /// `None` when the user recorded nothing for the work.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn user_title_state(
        &self,
        user_id: i64,
        work_id: &str,
    ) -> Result<Option<UserTitleState>> {
        Ok(
            user_title_state::Entity::find_by_id((user_id, work_id.to_string()))
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?
                .map(super::user_title_state::map_state),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored holding row with the given holder columns.
    fn holding(
        kind: &str,
        plugin: Option<&str>,
        account: Option<&str>,
        node: Option<&str>,
    ) -> holdings::Model {
        holdings::Model {
            id: "h1".into(),
            listing_id: "l1".into(),
            holder: String::new(),
            holder_kind: kind.into(),
            plugin_key: plugin.map(Into::into),
            account_id: account.map(Into::into),
            node_id: node.map(Into::into),
            entitlement: "purchase".into(),
            purchased_at: None,
            seen_pass: None,
            share_ref: None,
            share_title: None,
            share_digest: None,
            delivered_digest: None,
            delivered_at: None,
            created_at: "2026-11-14T00:00:00Z".into(),
            updated_at: "2026-11-14T00:00:00Z".into(),
        }
    }

    #[test]
    fn an_unknown_acquire_status_is_skipped_not_read_as_never_acquired() {
        assert_eq!(edition_status(None), Some(AcquireStatus::NotAcquired));
        assert_eq!(
            edition_status(Some(&"acquired".to_string())),
            Some(AcquireStatus::Acquired)
        );
        assert_eq!(edition_status(Some(&"archived".to_string())), None);
    }

    #[test]
    fn holdings_read_their_holder_or_are_skipped() {
        let account = listing_holding(holding("account", Some("audible"), Some("a1"), None));
        assert_eq!(
            account.map(|h| h.holder),
            Some(HoldingHolder::Account {
                plugin_key: "audible".into(),
                account_id: "a1".into()
            })
        );
        let node = listing_holding(holding("node", None, None, Some("n1")));
        assert_eq!(
            node.map(|h| h.holder),
            Some(HoldingHolder::Node {
                node_id: "n1".into()
            })
        );
        assert!(listing_holding(holding("account", Some("audible"), None, None)).is_none());
        assert!(listing_holding(holding("user", None, None, None)).is_none());
        let mut bad = holding("node", None, None, Some("n1"));
        bad.entitlement = "rental".into();
        assert!(listing_holding(bad).is_none());
    }
}
