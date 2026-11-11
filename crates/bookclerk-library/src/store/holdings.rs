//! Holdings an account loses: titles a scan no longer lists, accounts that
//! are revoked, and account ids that are remapped.
//!
//! A holding is added by the library model's link batch
//! ([`super::library_model`]) with a `holding_added` event. This module is
//! the other half: every removal deletes the row and publishes
//! `holding_removed` in the same atomic batch, so the search index and
//! other consumers hear about each one exactly once.
//!
//! Scans mark what they list: each written page stamps the pass id
//! (`account_scan_state.pass_id`) on the account's holdings of the page's
//! products ([`LibraryStore::mark_holdings_seen`]). When a complete full
//! pass ends, the account's holdings that no page stamped are the titles it
//! no longer holds ([`HoldingSelection::UnseenBy`]). A delta pass instead
//! names the products that left the account
//! ([`HoldingSelection::Products`]).
//!
//! Removals read the matching holdings in keyset pages and then delete
//! each one under a guard that repeats the selection, so a holding that a
//! concurrent pass stamped, or another writer already removed, is left
//! alone and its event is not published.

use sea_orm::sea_query::Query;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use uuid::Uuid;

use super::write_batches::WriteBatches;
use super::LibraryStore;
use crate::control_plane::{batch, LiveTopic};
use crate::entities::{holdings, store_listings};
use crate::error::{LibraryError, Result};

/// Event type published when a holding is removed.
pub const HOLDING_REMOVED: &str = "holding_removed";

/// Event type of a new holding (the link batch publishes most of them).
const HOLDING_ADDED: &str = "holding_added";

/// Most holdings one keyset page of a removal reads.
const REMOVAL_PAGE: u64 = 200;

/// Which holdings of one account a removal takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldingSelection<'a> {
    /// The account's holdings of these products of its plugin (a scan page's
    /// `removed` ids).
    Products(&'a [String]),
    /// The account's holdings that the scan pass `pass_id` did not list.
    UnseenBy(&'a str),
    /// Every holding of the account (it was revoked).
    All,
}

/// One holding read for removal.
#[derive(Debug, Clone)]
struct Doomed {
    /// `holdings.id`.
    id: String,
    /// The listing held.
    listing_id: String,
    /// Stored holder key.
    holder: String,
}

impl LibraryStore {
    /// Stamps scan pass `pass_id` on the account's holdings of `product_ids`.
    ///
    /// A holding is stamped whether or not its listing resolved on this
    /// page, so a title the library model could not settle is never taken
    /// for one that left the account.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin (and the listings').
    /// * `account_id` - The scanned account.
    /// * `pass_id` - The pass in progress (`account_scan_state.pass_id`).
    /// * `product_ids` - The product ids the page listed.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch fails.
    pub async fn mark_holdings_seen(
        &self,
        plugin_key: &str,
        account_id: &str,
        pass_id: &str,
        product_ids: &[String],
    ) -> Result<()> {
        if product_ids.is_empty() || pass_id.is_empty() {
            return Ok(());
        }
        let mut batches = WriteBatches::new(self.db_capabilities(), "holding stamp");
        for chunk in product_ids.chunks(self.lookup_chunk()) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut params = vec![
                batch::text(pass_id),
                batch::text(plugin_key),
                batch::text(account_id),
                batch::text(plugin_key),
            ];
            params.extend(chunk.iter().map(|id| batch::text(id)));
            let stmt = batch::exec(
                &format!(
                    "UPDATE holdings SET seen_pass = ? \
                     WHERE plugin_key = ? AND account_id = ? AND listing_id IN \
                     (SELECT id FROM store_listings WHERE plugin_key = ? AND product_id IN ({marks}))"
                ),
                params,
            );
            if let Some(full) = batches.push(stmt)? {
                self.run_holdings_batch("holdings-seen", full).await?;
            }
        }
        if let Some(last) = batches.finish() {
            self.run_holdings_batch("holdings-seen", last).await?;
        }
        Ok(())
    }

    /// Removes the account's holdings that `selection` names, publishing
    /// `holding_removed` for each in the batch that deletes it.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin.
    /// * `account_id` - The account.
    /// * `selection` - Which of its holdings go.
    ///
    /// # Returns
    ///
    /// How many holdings were selected for removal (a concurrent writer may
    /// have kept or removed some of them first).
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a batch fails; holdings removed by
    /// earlier batches stay removed.
    pub async fn remove_account_holdings(
        &self,
        plugin_key: &str,
        account_id: &str,
        selection: HoldingSelection<'_>,
    ) -> Result<usize> {
        let mut total = 0;
        match selection {
            HoldingSelection::Products(ids) => {
                for chunk in ids.chunks(self.lookup_chunk()) {
                    let listings = Query::select()
                        .column(store_listings::Column::Id)
                        .from(store_listings::Entity)
                        .and_where(store_listings::Column::PluginKey.eq(plugin_key))
                        .and_where(store_listings::Column::ProductId.is_in(chunk.iter().cloned()))
                        .to_owned();
                    let cond =
                        Condition::all().add(holdings::Column::ListingId.in_subquery(listings));
                    total += self
                        .remove_selected(plugin_key, account_id, cond, None)
                        .await?;
                }
            }
            HoldingSelection::UnseenBy(pass_id) => {
                let cond = Condition::any()
                    .add(holdings::Column::SeenPass.is_null())
                    .add(holdings::Column::SeenPass.ne(pass_id));
                total += self
                    .remove_selected(plugin_key, account_id, cond, Some(pass_id))
                    .await?;
            }
            HoldingSelection::All => {
                total += self
                    .remove_selected(plugin_key, account_id, Condition::all(), None)
                    .await?;
            }
        }
        Ok(total)
    }

    /// Reads the account's holdings matching `cond` in keyset pages and
    /// removes each; `unseen_by` repeats the pass guard in the delete.
    async fn remove_selected(
        &self,
        plugin_key: &str,
        account_id: &str,
        cond: Condition,
        unseen_by: Option<&str>,
    ) -> Result<usize> {
        let mut after = String::new();
        let mut total = 0;
        loop {
            let page: Vec<Doomed> = holdings::Entity::find()
                .filter(holdings::Column::PluginKey.eq(plugin_key))
                .filter(holdings::Column::AccountId.eq(account_id))
                .filter(holdings::Column::Id.gt(after.clone()))
                .filter(cond.clone())
                .order_by_asc(holdings::Column::Id)
                .limit(self.removal_page())
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .into_iter()
                .map(|m| Doomed {
                    id: m.id,
                    listing_id: m.listing_id,
                    holder: m.holder,
                })
                .collect();
            let Some(last) = page.last() else {
                return Ok(total);
            };
            after.clone_from(&last.id);
            total += page.len();
            let full = u64::try_from(page.len()).unwrap_or(u64::MAX) >= self.removal_page();
            self.delete_holdings(plugin_key, account_id, &page, unseen_by)
                .await?;
            if !full {
                return Ok(total);
            }
        }
    }

    /// Deletes `doomed`, each with its `holding_removed` event, in batches
    /// sized from the database plugin's limits.
    async fn delete_holdings(
        &self,
        plugin_key: &str,
        account_id: &str,
        doomed: &[Doomed],
        unseen_by: Option<&str>,
    ) -> Result<()> {
        let now = super::now_str();
        let mut batches = WriteBatches::new(self.db_capabilities(), "holding removal")
            .with_trailer(crate::control_plane::bump(LiveTopic::Library))?;
        for holding in doomed {
            let (guard, guard_binds) = match unseen_by {
                Some(pass) => (
                    "id = ? AND (seen_pass IS NULL OR seen_pass <> ?)",
                    vec![batch::text(&holding.id), batch::text(pass)],
                ),
                None => ("id = ?", vec![batch::text(&holding.id)]),
            };
            let event = holding_event(
                HOLDING_REMOVED,
                plugin_key,
                account_id,
                holding,
                &now,
                guard,
                guard_binds.clone(),
            );
            let delete = batch::exec(&format!("DELETE FROM holdings WHERE {guard}"), guard_binds);
            if let Some(full) = batches.push_group(vec![event, delete])? {
                self.run_holdings_batch("holdings-removed", full).await?;
            }
        }
        if let Some(last) = batches.finish() {
            self.run_holdings_batch("holdings-removed", last).await?;
        }
        Ok(())
    }

    /// Moves account `from`'s holdings to account `to` of the same plugin
    /// (an account id remap): each moves under a new id with
    /// `holding_removed` for the old holder and `holding_added` for the new
    /// one, and a holding `to` already has of the same listing is kept
    /// instead (the old one is removed).
    ///
    /// `to` must already be an account row, since the holding's foreign key
    /// names it.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a batch fails; holdings moved by
    /// earlier batches stay moved.
    pub(crate) async fn move_account_holdings(
        &self,
        plugin_key: &str,
        from: &str,
        to: &str,
    ) -> Result<usize> {
        let to_holder = format!("account:{to}");
        let mut total = 0;
        loop {
            let page = holdings::Entity::find()
                .filter(holdings::Column::PluginKey.eq(plugin_key))
                .filter(holdings::Column::AccountId.eq(from))
                .order_by_asc(holdings::Column::Id)
                .limit(self.removal_page())
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if page.is_empty() {
                return Ok(total);
            }
            total += page.len();
            let now = super::now_str();
            let mut batches = WriteBatches::new(self.db_capabilities(), "holding move")
                .with_trailer(crate::control_plane::bump(LiveTopic::Library))?;
            for old in page {
                let doomed = Doomed {
                    id: old.id.clone(),
                    listing_id: old.listing_id.clone(),
                    holder: old.holder.clone(),
                };
                let new_id = Uuid::new_v4().to_string();
                let mut group = vec![holding_event(
                    HOLDING_REMOVED,
                    plugin_key,
                    from,
                    &doomed,
                    &now,
                    "id = ?",
                    vec![batch::text(&old.id)],
                )];
                group.push(batch::exec(
                    "UPDATE holdings SET id = ?, holder = ?, account_id = ?, updated_at = ? \
                     WHERE id = ? AND NOT EXISTS \
                     (SELECT 1 FROM holdings WHERE listing_id = ? AND holder = ?)",
                    vec![
                        batch::text(&new_id),
                        batch::text(&to_holder),
                        batch::text(to),
                        batch::text(&now),
                        batch::text(&old.id),
                        batch::text(&old.listing_id),
                        batch::text(&to_holder),
                    ],
                ));
                let moved = Doomed {
                    id: new_id.clone(),
                    listing_id: old.listing_id.clone(),
                    holder: to_holder.clone(),
                };
                group.push(holding_event(
                    HOLDING_ADDED,
                    plugin_key,
                    to,
                    &moved,
                    &now,
                    "id = ?",
                    vec![batch::text(&new_id)],
                ));
                group.push(batch::exec(
                    "DELETE FROM holdings WHERE id = ?",
                    vec![batch::text(&old.id)],
                ));
                if let Some(full) = batches.push_group(group)? {
                    self.run_holdings_batch("holdings-moved", full).await?;
                }
            }
            if let Some(last) = batches.finish() {
                self.run_holdings_batch("holdings-moved", last).await?;
            }
        }
    }

    /// Holdings one keyset page reads: [`REMOVAL_PAGE`], or fewer when the
    /// backend returns fewer rows per statement.
    fn removal_page(&self) -> u64 {
        u64::from(self.max_result_rows).clamp(1, REMOVAL_PAGE)
    }

    /// Sends one batch of holding writes.
    async fn run_holdings_batch(
        &self,
        prefix: &str,
        statements: Vec<bookclerk_plugin_abi::TypedDbStatement>,
    ) -> Result<()> {
        let operation_id = format!("{prefix}-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }
}

/// The outbox insert of a holding event for `holding`, published only while
/// a holding matches `guard` (so a holding already gone, or kept by a
/// concurrent writer, announces nothing). The dedup key names the holding
/// id, which a holding never shares with an earlier one.
fn holding_event(
    event_type: &str,
    plugin_key: &str,
    account_id: &str,
    holding: &Doomed,
    now: &str,
    guard: &str,
    guard_binds: Vec<bookclerk_plugin_abi::DbValue>,
) -> bookclerk_plugin_abi::TypedDbStatement {
    let dedup_key = format!("{event_type}:{}", holding.id);
    let payload = serde_json::json!({
        "listingId": holding.listing_id,
        "holder": holding.holder,
        "holdingId": holding.id,
    })
    .to_string();
    let mut binds = vec![
        batch::text(&Uuid::new_v4().to_string()),
        batch::text(event_type),
        batch::text(now),
        batch::text(account_id),
        batch::text(plugin_key),
        batch::text(&holding.listing_id),
        batch::text(&dedup_key),
        batch::text(&payload),
        batch::text(&holding.listing_id),
        batch::text(now),
        batch::text(account_id),
        batch::text(plugin_key),
        batch::text(event_type),
        batch::text(&dedup_key),
    ];
    binds.extend(guard_binds);
    batch::exec(
        &format!(
            "INSERT OR IGNORE INTO domain_events (\
                id, event_type, schema_version, occurred_at, account_id, source, \
                correlation_id, causation_id, dedup_key, payload, ordering_key, \
                dispatch_state, created_at, wake_pending\
             ) SELECT ?, ?, 1, ?, ?, ?, ?, '', ?, ?, ?, 'pending', ?, 1 \
             WHERE NOT EXISTS (\
                SELECT 1 FROM domain_events \
                WHERE queue_name = 'core.events' AND account_id = ? AND source = ? \
                  AND event_type = ? AND dedup_key = ?\
             ) AND EXISTS (SELECT 1 FROM holdings WHERE {guard})"
        ),
        binds,
    )
}

#[cfg(test)]
mod tests {
    use bookclerk_resolve as resolve;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    use super::*;
    use crate::entities::{accounts, domain_events};
    use crate::test_support::{sqlite, Profile};

    /// The plugin every test listing is sold by.
    const PLUGIN: &str = "plugin:audible";

    /// An audiobook listing of `product`, titled after it.
    fn listing(product: &str) -> resolve::Listing {
        resolve::Listing::from_draft(&resolve::ListingDraft {
            plugin_key: PLUGIN.into(),
            marketplace: Some("us".into()),
            product_id: product.into(),
            work_kind: Some(resolve::WorkKind::Book),
            expression_kind: Some(resolve::ExpressionKind::Narration),
            media_kind: Some(resolve::MediaKind::Audiobook),
            title: format!("Title {product}"),
            identifiers: vec![(resolve::IdentifierScheme::Asin, product.into(), None)],
            ..resolve::ListingDraft::default()
        })
        .unwrap()
    }

    /// A purchase held by account `id`.
    fn held_by(id: &str) -> resolve::HoldingDraft {
        resolve::HoldingDraft {
            holder: resolve::Holder::Account(id.into()),
            entitlement: resolve::Entitlement::Purchase,
            purchased_at: None,
        }
    }

    /// A store under `profile` with account `a1` holding `products`.
    async fn library(profile: Profile, products: &[&str]) -> LibraryStore {
        let store = sqlite::memory(profile).await.into_store();
        store
            .upsert_account(PLUGIN, "a1", "us", None, true, "audible")
            .await
            .unwrap();
        for product in products {
            store
                .record_listing(&listing(product), &held_by("a1"))
                .await
                .unwrap();
        }
        store
    }

    /// `(product id, holder)` of every holding, sorted.
    async fn held(store: &LibraryStore) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for h in holdings::Entity::find().all(&store.db).await.unwrap() {
            let listing = store_listings::Entity::find_by_id(h.listing_id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            out.push((listing.product_id, h.holder));
        }
        out.sort();
        out
    }

    /// `(event type, holder)` of every holding event, in publish order.
    async fn holding_events(store: &LibraryStore) -> Vec<(String, String)> {
        domain_events::Entity::find()
            .filter(domain_events::Column::EventType.is_in([HOLDING_ADDED, HOLDING_REMOVED]))
            .order_by_asc(domain_events::Column::CreatedAt)
            .order_by_asc(domain_events::Column::EventType)
            .all(&store.db)
            .await
            .unwrap()
            .into_iter()
            .map(|e| {
                let payload: serde_json::Value = serde_json::from_str(&e.payload).unwrap();
                (
                    e.event_type,
                    payload["holder"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Count of events of `event_type`.
    async fn count(store: &LibraryStore, event_type: &str) -> usize {
        domain_events::Entity::find()
            .filter(domain_events::Column::EventType.eq(event_type))
            .all(&store.db)
            .await
            .unwrap()
            .len()
    }

    fn ids(products: &[&str]) -> Vec<String> {
        products.iter().map(|p| (*p).to_string()).collect()
    }

    /// A holding names its account in typed columns, and is never written
    /// for an account the library does not have.
    #[tokio::test]
    async fn a_holding_carries_its_typed_holder_and_needs_its_account() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1"]).await;
            let row = holdings::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.holder, "account:a1");
            assert_eq!(row.holder_kind, "account");
            assert_eq!(row.plugin_key.as_deref(), Some(PLUGIN));
            assert_eq!(row.account_id.as_deref(), Some("a1"));
            assert_eq!(row.node_id, None);

            store
                .record_listing(&listing("B2"), &held_by("ghost"))
                .await
                .unwrap();
            assert_eq!(held(&store).await.len(), 1, "{profile:?}: no ghost holding");
            assert_eq!(count(&store, HOLDING_ADDED).await, 1, "{profile:?}");
        }
    }

    /// Deleting an account row drops its holdings through the foreign key.
    #[tokio::test]
    async fn deleting_an_account_cascades_to_its_holdings() {
        let store = library(Profile::FirstParty, &["B1", "B2"]).await;
        accounts::Entity::delete_many()
            .filter(accounts::Column::AccountId.eq("a1"))
            .exec(&store.db)
            .await
            .unwrap();
        assert!(held(&store).await.is_empty());
    }

    /// A delta page's removed ids drop exactly those holdings, each with
    /// one `holding_removed`; a second removal publishes nothing.
    #[tokio::test]
    async fn removed_products_lose_their_holdings_once() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2", "B3"]).await;
            let gone = ids(&["B2", "B3", "never-held"]);
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::Products(&gone))
                .await
                .unwrap();
            assert_eq!(n, 2, "{profile:?}");
            assert_eq!(
                held(&store).await,
                [("B1".to_string(), "account:a1".to_string())]
            );
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
            store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::Products(&gone))
                .await
                .unwrap();
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
        }
    }

    /// A complete full pass removes the holdings it did not stamp, and
    /// leaves those it did, including one stamped by a later pass.
    #[tokio::test]
    async fn a_full_pass_removes_what_it_did_not_list() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2", "B3", "B4"]).await;
            store
                .mark_holdings_seen(PLUGIN, "a1", "pass-1", &ids(&["B1", "B3"]))
                .await
                .unwrap();
            // A concurrent pass listed B4 after this pass read it.
            store
                .mark_holdings_seen(PLUGIN, "a1", "pass-2", &ids(&["B4"]))
                .await
                .unwrap();
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::UnseenBy("pass-1"))
                .await
                .unwrap();
            assert_eq!(n, 2, "{profile:?}: B2 and B4 read as unseen");
            assert_eq!(
                held(&store)
                    .await
                    .into_iter()
                    .map(|(p, _)| p)
                    .collect::<Vec<_>>(),
                ["B1", "B3"],
                "{profile:?}"
            );
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
        }
    }

    /// The guard re-checks the pass: a holding stamped between the read
    /// and the delete stays, and publishes nothing.
    #[tokio::test]
    async fn a_holding_stamped_after_the_read_is_kept() {
        let store = library(Profile::FirstParty, &["B1"]).await;
        let row = holdings::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        store
            .mark_holdings_seen(PLUGIN, "a1", "pass-1", &ids(&["B1"]))
            .await
            .unwrap();
        let doomed = [Doomed {
            id: row.id,
            listing_id: row.listing_id,
            holder: row.holder,
        }];
        store
            .delete_holdings(PLUGIN, "a1", &doomed, Some("pass-1"))
            .await
            .unwrap();
        assert_eq!(held(&store).await.len(), 1);
        assert_eq!(count(&store, HOLDING_REMOVED).await, 0);
    }

    /// Many holdings are removed in keyset pages and size-limited batches.
    #[tokio::test]
    async fn a_large_removal_pages_through_every_holding() {
        let products: Vec<String> = (0..30).map(|i| format!("P{i:02}")).collect();
        let refs: Vec<&str> = products.iter().map(String::as_str).collect();
        for profile in Profile::ALL {
            let store = library(profile, &refs).await;
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::All)
                .await
                .unwrap();
            assert_eq!(n, 30, "{profile:?}");
            assert!(held(&store).await.is_empty(), "{profile:?}");
            assert_eq!(count(&store, HOLDING_REMOVED).await, 30, "{profile:?}");
        }
    }

    /// A title removed and held again publishes `holding_added` again.
    #[tokio::test]
    async fn a_holding_held_again_announces_itself_again() {
        let store = library(Profile::D1, &["B1"]).await;
        store
            .remove_account_holdings(PLUGIN, "a1", HoldingSelection::All)
            .await
            .unwrap();
        store
            .record_listing(&listing("B1"), &held_by("a1"))
            .await
            .unwrap();
        assert_eq!(
            holding_events(&store).await,
            [
                (HOLDING_ADDED.to_string(), "account:a1".to_string()),
                (HOLDING_REMOVED.to_string(), "account:a1".to_string()),
                (HOLDING_ADDED.to_string(), "account:a1".to_string()),
            ]
        );
    }

    /// Revoking an account removes its holdings and its sync token, so the
    /// next pass is full and adds them back.
    #[tokio::test]
    async fn revoking_an_account_removes_its_holdings_and_token() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2"]).await;
            store
                .finish_scan_pass(PLUGIN, "a1", "tok", true)
                .await
                .unwrap();
            store.revoke_credentials(PLUGIN, "a1").await.unwrap();
            assert!(held(&store).await.is_empty(), "{profile:?}");
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
            let state = store.scan_state(PLUGIN, "a1").await.unwrap().unwrap();
            assert_eq!(state.sync_token, "", "{profile:?}");
        }
    }

    /// A remap moves holdings to the new id with an event pair each, and a
    /// holding the new id already has is kept rather than duplicated.
    #[tokio::test]
    async fn remapping_an_account_moves_its_holdings() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2"]).await;
            store
                .upsert_account(PLUGIN, "a2", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .record_listing(&listing("B2"), &held_by("a2"))
                .await
                .unwrap();
            store.remap_account_id(PLUGIN, "a1", "a2").await.unwrap();
            assert_eq!(
                held(&store).await,
                [
                    ("B1".to_string(), "account:a2".to_string()),
                    ("B2".to_string(), "account:a2".to_string()),
                ],
                "{profile:?}"
            );
            let moved = holdings::Entity::find()
                .filter(holdings::Column::AccountId.eq("a2"))
                .all(&store.db)
                .await
                .unwrap();
            assert!(moved.iter().all(|h| h.account_id.as_deref() == Some("a2")));
            // B1 and B2 each leave a1; only B1 is new for a2.
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
            assert_eq!(
                count(&store, HOLDING_ADDED).await,
                4,
                "{profile:?}: a1's two, a2's B2, and B1 moved"
            );
        }
    }

    /// Stamping reaches only the account's own holdings.
    #[tokio::test]
    async fn stamping_is_scoped_to_the_account() {
        let store = library(Profile::FirstParty, &["B1"]).await;
        store
            .upsert_account(PLUGIN, "a2", "us", None, true, "audible")
            .await
            .unwrap();
        store
            .record_listing(&listing("B1"), &held_by("a2"))
            .await
            .unwrap();
        store
            .mark_holdings_seen(PLUGIN, "a1", "pass-1", &ids(&["B1"]))
            .await
            .unwrap();
        let rows = holdings::Entity::find()
            .order_by_asc(holdings::Column::Holder)
            .all(&store.db)
            .await
            .unwrap();
        let seen: Vec<_> = rows.iter().map(|h| h.seen_pass.as_deref()).collect();
        assert_eq!(seen, [Some("pass-1"), None]);
    }
}
