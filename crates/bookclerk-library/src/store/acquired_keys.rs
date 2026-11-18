//! The account titles whose edition is acquired with a stored file, by
//! identity only, in keyset pages (book model step 6, PR 4e-5).
//!
//! A branch's catalog sync walks every such title on each pass and reads a
//! title's full view ([`LibraryStore::acquire_title_of_holding`]) only when
//! it sends it, so a pass never holds every title's metadata at once.

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::LibraryStore;
use crate::entities::{accounts, edition_acquisitions, holdings, store_listings};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// Upper clamp for the editions one [`LibraryStore::acquired_title_keys_page`]
/// read covers.
///
/// An acquisition row is a few short columns (well under 1 KiB on the wire),
/// so this many stay far below the 256 KiB inline result cap of an adapter
/// that does not stream.
pub const ACQUIRED_KEYS_MAX_PAGE: u64 = 256;

/// One account's title of an edition acquired with a stored file, without
/// its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquiredTitleKey {
    /// The account holding (`holdings.id`): the id the full view is read
    /// by.
    pub holding_id: String,
    /// The acquired edition (`editions.id`).
    pub edition_id: String,
    /// Display alias of the store plugin (`audible`, `libro`, ...), from
    /// the account; empty when the account row is gone.
    pub source: String,
    /// Provenance-qualified plugin key of the store.
    pub plugin_key: String,
    /// Store account that holds the title.
    pub account_id: String,
    /// Store marketplace of the listing, as stored.
    pub marketplace: String,
    /// Store product id of the listing.
    pub product_id: String,
    /// Last change of the title: the latest of its holding, its listing and
    /// its edition's acquire state. An unparsable stored value counts as
    /// the time of the read.
    pub updated_at: DateTime<Utc>,
}

/// One page of [`AcquiredTitleKey`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcquiredTitleKeyPage {
    /// Every account title of the page's editions, by edition then holding
    /// id.
    pub keys: Vec<AcquiredTitleKey>,
    /// The cursor of the next page (an edition id), or `None` after the
    /// last.
    pub next: Option<String>,
}

/// `text` as a time, or the time of the read when it does not parse.
fn changed_at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text.trim())
        .map_or_else(|_| Utc::now(), |at| at.with_timezone(&Utc))
}

impl LibraryStore {
    /// One keyset page of the account titles whose edition is acquired
    /// with a stored audio file, identity columns only.
    ///
    /// # Arguments
    ///
    /// * `after` - The previous page's [`AcquiredTitleKeyPage::next`], or
    ///   `None` for the first page.
    /// * `limit` - Editions wanted, clamped to
    ///   1..=[`ACQUIRED_KEYS_MAX_PAGE`] and to the database's row limit.
    ///
    /// # Returns
    ///
    /// Every account holding of up to `limit` acquired editions, in edition
    /// then holding order. An acquired edition no account holds yields no
    /// key; walk until `next` is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquired_title_keys_page(
        &self,
        after: Option<&str>,
        limit: u64,
    ) -> Result<AcquiredTitleKeyPage> {
        use super::library_model::{find_chunked, find_chunked_paged};
        let want = match self.db_capabilities().max_result_rows {
            0 => limit.clamp(1, ACQUIRED_KEYS_MAX_PAGE),
            rows => limit
                .clamp(1, ACQUIRED_KEYS_MAX_PAGE)
                .min(u64::from(rows).max(1)),
        };
        let mut query = edition_acquisitions::Entity::find()
            .filter(edition_acquisitions::Column::Status.eq(AcquireStatus::Acquired.as_str()))
            .filter(edition_acquisitions::Column::StorageKey.is_not_null())
            .order_by_asc(edition_acquisitions::Column::EditionId)
            .limit(want);
        if let Some(after) = after {
            query = query.filter(edition_acquisitions::Column::EditionId.gt(after));
        }
        let acquired = query.all(self.db()).await.map_err(LibraryError::Orm)?;
        let full = u64::try_from(acquired.len()).unwrap_or(u64::MAX) >= want;
        let next = acquired
            .last()
            .filter(|_| full)
            .map(|row| row.edition_id.clone());
        if acquired.is_empty() {
            return Ok(AcquiredTitleKeyPage {
                keys: Vec::new(),
                next,
            });
        }
        let edition_ids: Vec<String> = acquired.iter().map(|row| row.edition_id.clone()).collect();
        let listings =
            find_chunked_paged::<store_listings::Entity, _>(self, &edition_ids, |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.to_vec()))
                    .order_by_asc(store_listings::Column::Id)
            })
            .await?;
        let listing_ids: Vec<String> = listings.iter().map(|l| l.id.clone()).collect();
        let holdings = find_chunked_paged::<holdings::Entity, _>(self, &listing_ids, |part| {
            super::holdings::find_holdings()
                .filter(holdings::Column::HolderKind.eq("account"))
                .filter(holdings::Column::ListingId.is_in(part.to_vec()))
                .order_by_asc(holdings::Column::Id)
        })
        .await?;
        let mut account_ids: Vec<String> = holdings
            .iter()
            .filter_map(|h| h.account_id.clone())
            .collect();
        account_ids.sort_unstable();
        account_ids.dedup();
        let sources: std::collections::BTreeMap<(String, String), String> =
            find_chunked::<accounts::Entity, _>(self, &account_ids, |part| {
                accounts::Entity::find().filter(accounts::Column::AccountId.is_in(part.to_vec()))
            })
            .await?
            .into_iter()
            .map(|m| ((m.plugin_key, m.account_id), m.source))
            .collect();
        let acquired: std::collections::BTreeMap<&str, &edition_acquisitions::Model> = acquired
            .iter()
            .map(|row| (row.edition_id.as_str(), row))
            .collect();
        let listings: std::collections::BTreeMap<&str, &store_listings::Model> =
            listings.iter().map(|l| (l.id.as_str(), l)).collect();
        let mut keys: Vec<AcquiredTitleKey> = holdings
            .into_iter()
            .filter_map(|holding| {
                let plugin_key = holding.plugin_key?;
                let account_id = holding.account_id?;
                let listing = listings.get(holding.listing_id.as_str())?;
                let acquisition = acquired.get(listing.edition_id.as_str())?;
                let updated_at = changed_at(&holding.updated_at)
                    .max(changed_at(&listing.updated_at))
                    .max(changed_at(&acquisition.updated_at));
                Some(AcquiredTitleKey {
                    holding_id: holding.id,
                    edition_id: listing.edition_id.clone(),
                    source: sources
                        .get(&(plugin_key.clone(), account_id.clone()))
                        .cloned()
                        .unwrap_or_default(),
                    plugin_key,
                    account_id,
                    marketplace: listing.marketplace.clone(),
                    product_id: listing.product_id.clone(),
                    updated_at,
                })
            })
            .collect();
        keys.sort_by(|a, b| {
            (a.edition_id.as_str(), a.holding_id.as_str())
                .cmp(&(b.edition_id.as_str(), b.holding_id.as_str()))
        });
        Ok(AcquiredTitleKeyPage { keys, next })
    }
}

#[cfg(test)]
impl LibraryStore {
    /// Every [`AcquiredTitleKey`], walking
    /// [`Self::acquired_title_keys_page`] to its end.
    pub(crate) async fn all_acquired_title_keys(&self) -> Vec<AcquiredTitleKey> {
        let mut keys = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self
                .acquired_title_keys_page(after.as_deref(), ACQUIRED_KEYS_MAX_PAGE)
                .await
                .expect("a page of acquired title keys");
            keys.extend(page.keys);
            match page.next {
                Some(next) => after = Some(next),
                None => return keys,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::models::AcquireStatus;
    use crate::store::NewBook;
    use crate::test_support::Profile;

    #[tokio::test]
    async fn a_walk_of_pages_lists_every_title_of_an_acquired_edition_under_every_profile() {
        for profile in Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            for account in ["acct", "other"] {
                store
                    .upsert_account("audible", account, "us", None, true, "audible")
                    .await
                    .unwrap();
            }
            for i in 0..9 {
                let product = format!("B0KEY{i:04}");
                store
                    .upsert_book(&NewBook::minimal(&product, "acct", "us", &product))
                    .await
                    .unwrap();
                let (status, key) = match i % 3 {
                    0 => (AcquireStatus::Acquired, Some(format!("k/{product}.m4b"))),
                    1 => (AcquireStatus::Acquired, None),
                    _ => (AcquireStatus::NotAcquired, None),
                };
                store
                    .set_acquire_status(&product, "acct", status, key.as_deref(), None)
                    .await
                    .unwrap();
            }
            // The other account holds an acquired edition too: its title
            // is listed beside the one whose acquire wrote the state.
            store
                .upsert_book(&NewBook::minimal("B0KEY0003", "other", "us", "B0KEY0003"))
                .await
                .unwrap();

            for size in [1, 2, 100] {
                let mut keys = Vec::new();
                let mut after: Option<String> = None;
                loop {
                    let page = store
                        .acquired_title_keys_page(after.as_deref(), size)
                        .await
                        .unwrap();
                    keys.extend(page.keys);
                    match page.next {
                        Some(next) => after = Some(next),
                        None => break,
                    }
                }
                let mut held: Vec<_> = keys
                    .iter()
                    .map(|k| (k.account_id.as_str(), k.product_id.as_str()))
                    .collect();
                held.sort_unstable();
                assert_eq!(
                    held,
                    vec![
                        ("acct", "B0KEY0000"),
                        ("acct", "B0KEY0003"),
                        ("acct", "B0KEY0006"),
                        ("other", "B0KEY0003"),
                    ],
                    "{name}, pages of {size}"
                );
                for key in &keys {
                    assert_eq!(key.source, "audible", "{name}");
                    let title = store
                        .acquire_title_of_holding(&key.holding_id)
                        .await
                        .unwrap()
                        .expect("the holding's title");
                    assert_eq!(title.edition_id, key.edition_id, "{name}");
                    assert_eq!(title.status, AcquireStatus::Acquired, "{name}");
                }
            }
        }
    }
}
