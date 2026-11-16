//! An Administrator's resolution decisions (`resolution_overrides`) and the
//! conflicts the resolver left for one (`resolution_conflicts`).
//!
//! A pin says which edition a store listing sells, whatever its identifiers
//! say. Pinning moves the listing (and so its holdings and offers) to that
//! edition in the same atomic batch that records the pin, drops the
//! listing's recorded conflicts, and publishes `edition_linked` for the
//! edition it joined and `edition_updated` for the one it left, so the
//! library page index and the works search index refresh both works. The
//! `edition_linked` dedup key carries the pin's revision, so moving a
//! listing back to an edition it sold before publishes again (#639).
//!
//! Writes go to the database plugin as one atomic batch, never an
//! interactive transaction (`docs/adr/library-data-model.md`).

use bookclerk_plugin_abi::{DbValue, TypedDbStatement};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{now_str, LibraryStore};
use crate::control_plane::batch;
use crate::entities::{editions, resolution_conflicts, resolution_overrides, store_listings};
use crate::error::{LibraryError, Result};

/// Largest conflicts page [`LibraryStore::list_resolution_conflicts`] reads.
pub const MAX_CONFLICT_PAGE: u64 = 200;

/// What [`LibraryStore::pin_listing`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinOutcome {
    /// The listing now sells the pinned edition; it sold `from` before.
    Moved {
        /// The edition the listing left.
        from: String,
    },
    /// The listing already sold the pinned edition; the pin is recorded
    /// (or renewed) and nothing moved.
    Unchanged,
    /// No listing has this id.
    UnknownListing,
    /// No edition has this id.
    UnknownEdition,
}

/// One conflict the resolver recorded for an Administrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionConflict {
    /// The listing whose resolution conflicted.
    pub listing_id: String,
    /// `strong_identifiers`, `work_identifiers`, `ambiguous_work` or
    /// `work_kind_mismatch`.
    pub kind: String,
    /// The edition ids (`strong_identifiers`) or work ids (the other kinds)
    /// involved, sorted.
    pub subject_ids: Vec<String>,
    /// The listing's storefront PluginKey.
    pub plugin_key: String,
    /// The store's product id.
    pub product_id: String,
    /// The listing's title as the store reported it.
    pub title: String,
    /// The edition the listing sells now.
    pub edition_id: String,
    /// RFC 3339 time the conflict was last seen.
    pub updated_at: String,
}

/// Where a conflicts page stopped: the last row's key, in key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictCursor {
    /// The last row's listing id.
    pub listing_id: String,
    /// The last row's kind.
    pub kind: String,
    /// The last row's stored subject ids (a JSON array).
    pub subject_ids: String,
}

impl ConflictCursor {
    /// The cursor as an opaque URL-safe token.
    pub fn encode(&self) -> String {
        use base64::Engine as _;
        // Serializing a struct of strings cannot fail.
        let json = serde_json::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    /// Reads a token made by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when the token is not one.
    pub fn decode(token: &str) -> Result<Self> {
        use base64::Engine as _;
        let invalid = || LibraryError::Invalid("invalid page cursor".into());
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .map_err(|_| invalid())?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }
}

impl LibraryStore {
    /// Pins `listing_id` to `edition_id`: records the Administrator's
    /// decision and moves the listing there, as one atomic batch.
    ///
    /// Every statement requires both the listing and the edition to exist,
    /// so an unknown id writes nothing. The listing's recorded conflicts are
    /// dropped (the pin settles them). When the listing moves, the batch
    /// publishes `edition_linked` for `edition_id` and `edition_updated` for
    /// the edition it left; both are published only while this batch's pin
    /// is the stored one.
    ///
    /// # Arguments
    ///
    /// * `listing_id` - The listing (`store_listings.id`).
    /// * `edition_id` - The edition it sells (`editions.id`).
    /// * `user_id` - The Administrator (`users.id`); `None` for the operator.
    ///
    /// # Returns
    ///
    /// What the pin did.
    ///
    /// # Errors
    ///
    /// Returns the database error of a failed read or write.
    pub async fn pin_listing(
        &self,
        listing_id: &str,
        edition_id: &str,
        user_id: Option<i64>,
    ) -> Result<PinOutcome> {
        let Some(listing) = store_listings::Entity::find_by_id(listing_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(PinOutcome::UnknownListing);
        };
        let edition_known = editions::Entity::find_by_id(edition_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some();
        if !edition_known {
            return Ok(PinOutcome::UnknownEdition);
        }
        let from = listing.edition_id;
        let revision = Uuid::new_v4().to_string();
        let statements = pin_statements(&PinWrite {
            listing_id,
            edition_id,
            from: &from,
            source: &listing.plugin_key,
            user_id,
            revision: &revision,
            now: &now_str(),
        });
        let operation_id = format!("pin-listing-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        if from == edition_id {
            Ok(PinOutcome::Unchanged)
        } else {
            Ok(PinOutcome::Moved { from })
        }
    }

    /// Removes the pin on `listing_id`, if any.
    ///
    /// The listing keeps selling the edition it was pinned to: the resolver
    /// keeps a stored listing's edition, so a pin only ever moves a listing
    /// when it is made.
    ///
    /// # Returns
    ///
    /// Whether a pin was removed.
    ///
    /// # Errors
    ///
    /// Returns the database error of a failed read or write.
    pub async fn unpin_listing(&self, listing_id: &str) -> Result<bool> {
        let pinned = resolution_overrides::Entity::find()
            .filter(resolution_overrides::Column::SubjectKind.eq("listing"))
            .filter(resolution_overrides::Column::SubjectId.eq(listing_id))
            .filter(resolution_overrides::Column::Action.eq("pin_edition"))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some();
        if !pinned {
            return Ok(false);
        }
        let operation_id = format!("unpin-listing-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(
                "DELETE FROM resolution_overrides \
                 WHERE subject_kind = 'listing' AND subject_id = ? AND action = 'pin_edition'",
                vec![batch::text(listing_id)],
            )],
        ))
        .await?;
        Ok(true)
    }

    /// The edition `listing_id` is pinned to, or `None` when it has no pin.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn listing_pin(&self, listing_id: &str) -> Result<Option<String>> {
        Ok(resolution_overrides::Entity::find()
            .filter(resolution_overrides::Column::SubjectKind.eq("listing"))
            .filter(resolution_overrides::Column::SubjectId.eq(listing_id))
            .filter(resolution_overrides::Column::Action.eq("pin_edition"))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .and_then(|m| m.target_id))
    }

    /// One page of the conflicts the resolver recorded, in key order
    /// (listing, kind, subjects), with each listing's store and title.
    ///
    /// # Arguments
    ///
    /// * `after` - Where the previous page stopped; `None` starts at the
    ///   first conflict.
    /// * `limit` - Page size, clamped to 1..=[`MAX_CONFLICT_PAGE`] and to
    ///   the database plugin's result-row cap.
    ///
    /// # Returns
    ///
    /// The page, and the cursor of the next page when this one is full.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails, or
    /// [`LibraryError::Invalid`] when a stored subject list is not a JSON
    /// array of strings.
    pub async fn list_resolution_conflicts(
        &self,
        after: Option<&ConflictCursor>,
        limit: u64,
    ) -> Result<(Vec<ResolutionConflict>, Option<ConflictCursor>)> {
        let limit = limit
            .clamp(1, MAX_CONFLICT_PAGE)
            .min(self.result_row_cap_for_reads());
        let mut query = resolution_conflicts::Entity::find()
            .order_by_asc(resolution_conflicts::Column::ListingId)
            .order_by_asc(resolution_conflicts::Column::Kind)
            .order_by_asc(resolution_conflicts::Column::SubjectIds);
        if let Some(c) = after {
            query = query.filter(
                Condition::any()
                    .add(resolution_conflicts::Column::ListingId.gt(c.listing_id.as_str()))
                    .add(
                        Condition::all()
                            .add(resolution_conflicts::Column::ListingId.eq(c.listing_id.as_str()))
                            .add(resolution_conflicts::Column::Kind.gt(c.kind.as_str())),
                    )
                    .add(
                        Condition::all()
                            .add(resolution_conflicts::Column::ListingId.eq(c.listing_id.as_str()))
                            .add(resolution_conflicts::Column::Kind.eq(c.kind.as_str()))
                            .add(
                                resolution_conflicts::Column::SubjectIds.gt(c.subject_ids.as_str()),
                            ),
                    ),
            );
        }
        let rows = query
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let next = if u64::try_from(rows.len()).unwrap_or(u64::MAX) >= limit {
            rows.last().map(|r| ConflictCursor {
                listing_id: r.listing_id.clone(),
                kind: r.kind.clone(),
                subject_ids: r.subject_ids.clone(),
            })
        } else {
            None
        };
        let listing_ids: Vec<String> = rows
            .iter()
            .map(|r| r.listing_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let listings: std::collections::HashMap<String, store_listings::Model> =
            super::library_model::find_chunked::<store_listings::Entity, _>(
                self,
                &listing_ids,
                |part| {
                    store_listings::Entity::find()
                        .filter(store_listings::Column::Id.is_in(part.iter().cloned()))
                },
            )
            .await?
            .into_iter()
            .map(|l| (l.id.clone(), l))
            .collect();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let subject_ids: Vec<String> = serde_json::from_str(&row.subject_ids)
                .map_err(|_| LibraryError::Invalid("stored conflict subjects".into()))?;
            // A conflict row cascades with its listing, so a missing
            // listing is a concurrent delete: leave the row out.
            let Some(listing) = listings.get(&row.listing_id) else {
                continue;
            };
            out.push(ResolutionConflict {
                listing_id: row.listing_id,
                kind: row.kind,
                subject_ids,
                plugin_key: listing.plugin_key.clone(),
                product_id: listing.product_id.clone(),
                title: listing.title.clone(),
                edition_id: listing.edition_id.clone(),
                updated_at: row.updated_at,
            });
        }
        Ok((out, next))
    }

    /// Records a resolver conflict for `listing_id`, as a scan that could
    /// not decide would. Test support only.
    ///
    /// # Panics
    ///
    /// When the write fails.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::expect_used)] // Test support: a failed fixture write is a test failure.
    pub async fn record_resolution_conflict_for_test(
        &self,
        listing_id: &str,
        kind: &str,
        subject_ids: &[&str],
    ) {
        let mut ids: Vec<&str> = subject_ids.to_vec();
        ids.sort_unstable();
        let now = now_str();
        self.execute_host_batch(batch::request(
            &format!("conflict-fixture-{}", Uuid::new_v4()),
            vec![batch::exec(
                "INSERT OR IGNORE INTO resolution_conflicts \
                 (listing_id, kind, subject_ids, created_at, updated_at) SELECT ?, ?, ?, ?, ?",
                vec![
                    batch::text(listing_id),
                    batch::text(kind),
                    batch::text(&serde_json::to_string(&ids).unwrap_or_default()),
                    batch::text(&now),
                    batch::text(&now),
                ],
            )],
        ))
        .await
        .expect("conflict fixture");
    }

    /// Most rows one read may return: the database plugin's advertised
    /// `maxResultRows`, or no cap when it advertises none.
    fn result_row_cap_for_reads(&self) -> u64 {
        match self.db_capabilities().max_result_rows {
            0 => u64::MAX,
            rows => u64::from(rows),
        }
    }
}

/// Everything one pin writes.
struct PinWrite<'a> {
    /// The listing pinned.
    listing_id: &'a str,
    /// The edition it is pinned to.
    edition_id: &'a str,
    /// The edition it sold when the pin was read.
    from: &'a str,
    /// The listing's storefront PluginKey (the events' source).
    source: &'a str,
    /// The Administrator; `None` for the operator.
    user_id: Option<i64>,
    /// This pin's revision (a fresh UUID).
    revision: &'a str,
    /// RFC 3339 write time.
    now: &'a str,
}

/// The statements of one pin, every one guarded so an unknown listing or
/// edition writes nothing; the events also require this pin to be the
/// stored one, so a concurrent pin that won publishes only its own.
fn pin_statements(p: &PinWrite<'_>) -> Vec<TypedDbStatement> {
    let edition_exists = "EXISTS (SELECT 1 FROM editions WHERE id = ?)";
    let mut out = vec![
        batch::exec(
            &format!(
                "DELETE FROM resolution_overrides \
                 WHERE subject_kind = 'listing' AND subject_id = ? AND {edition_exists}"
            ),
            vec![batch::text(p.listing_id), batch::text(p.edition_id)],
        ),
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO resolution_overrides \
                 (subject_kind, subject_id, action, target_id, created_by_user_id, created_at, revision) \
                 SELECT 'listing', ?, 'pin_edition', ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM store_listings WHERE id = ?) AND {edition_exists}"
            ),
            vec![
                batch::text(p.listing_id),
                batch::text(p.edition_id),
                batch::int_opt(p.user_id),
                batch::text(p.now),
                batch::text(p.revision),
                batch::text(p.listing_id),
                batch::text(p.edition_id),
            ],
        ),
        batch::exec(
            &format!(
                "UPDATE store_listings SET edition_id = ?, updated_at = ? \
                 WHERE id = ? AND {edition_exists}"
            ),
            vec![
                batch::text(p.edition_id),
                batch::text(p.now),
                batch::text(p.listing_id),
                batch::text(p.edition_id),
            ],
        ),
        batch::exec(
            &format!("DELETE FROM resolution_conflicts WHERE listing_id = ? AND {edition_exists}"),
            vec![batch::text(p.listing_id), batch::text(p.edition_id)],
        ),
    ];
    if p.from != p.edition_id {
        let linked_key = format!(
            "edition_linked:{}:{}:{}",
            p.listing_id, p.edition_id, p.revision
        );
        out.push(pin_event(
            p,
            "edition_linked",
            &linked_key,
            serde_json::json!({ "editionId": p.edition_id, "listingId": p.listing_id }),
            p.edition_id,
        ));
        // The edition the listing left lost a listing; `edition_updated`
        // makes the consumers refresh its work.
        let left_key = format!("edition_updated:{}:{}", p.from, p.revision);
        out.push(pin_event(
            p,
            "edition_updated",
            &left_key,
            serde_json::json!({ "editionId": p.from }),
            p.from,
        ));
    }
    out
}

/// The outbox insert of one event a pin publishes, guarded on this pin's
/// revision being the stored one.
fn pin_event(
    p: &PinWrite<'_>,
    event_type: &str,
    dedup_key: &str,
    payload: serde_json::Value,
    ordering_key: &str,
) -> TypedDbStatement {
    let payload = payload.to_string();
    let binds: Vec<DbValue> = vec![
        batch::text(&Uuid::new_v4().to_string()),
        batch::text(event_type),
        batch::text(p.now),
        batch::text(p.source),
        batch::text(p.listing_id),
        batch::text(dedup_key),
        batch::text(&payload),
        batch::text(ordering_key),
        batch::text(p.now),
        batch::text(p.source),
        batch::text(event_type),
        batch::text(dedup_key),
        batch::text(p.listing_id),
        batch::text(p.revision),
    ];
    batch::exec(
        "INSERT OR IGNORE INTO domain_events (\
            id, event_type, schema_version, occurred_at, account_id, source, \
            correlation_id, causation_id, dedup_key, payload, ordering_key, \
            dispatch_state, created_at, wake_pending\
         ) SELECT ?, ?, 1, ?, '', ?, ?, '', ?, ?, ?, 'pending', ?, 1 \
         WHERE NOT EXISTS (\
            SELECT 1 FROM domain_events \
            WHERE queue_name = 'core.events' AND account_id = '' AND source = ? \
              AND event_type = ? AND dedup_key = ?\
         ) AND EXISTS (\
            SELECT 1 FROM resolution_overrides \
            WHERE subject_kind = 'listing' AND subject_id = ? AND revision = ?\
         )",
        binds,
    )
}

#[cfg(test)]
mod tests {
    use bookclerk_resolve as resolve;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    use super::*;
    use crate::entities::domain_events;
    use crate::test_support::{sqlite, Profile};

    /// The plugin every test listing is sold by.
    const PLUGIN: &str = "plugin:audible";

    /// A catalog audiobook listing of `product`, with its own ASIN (so its
    /// own edition).
    fn listing(product: &str) -> resolve::Listing {
        resolve::Listing::from_draft(&resolve::ListingDraft {
            plugin_key: PLUGIN.into(),
            marketplace: Some("us".into()),
            product_id: product.into(),
            work_kind: Some(resolve::WorkKind::Book),
            expression_kind: Some(resolve::ExpressionKind::Narration),
            media_kind: Some(resolve::MediaKind::Audio),
            title: format!("Title {product}"),
            identifiers: vec![(resolve::IdentifierScheme::Asin, product.into(), None)],
            ..resolve::ListingDraft::default()
        })
        .unwrap()
    }

    /// Records catalog listings of `a` and `b`; returns their listing and
    /// edition ids.
    async fn two_listings(store: &LibraryStore) -> ((String, String), (String, String)) {
        let a = store.record_catalog_listing(&listing("A1")).await.unwrap();
        let b = store.record_catalog_listing(&listing("B2")).await.unwrap();
        (
            (a.listing.id.clone(), a.listing.edition_id.clone()),
            (b.listing.id.clone(), b.listing.edition_id.clone()),
        )
    }

    /// The edition `listing_id` sells now.
    async fn edition_of(store: &LibraryStore, listing_id: &str) -> String {
        store_listings::Entity::find_by_id(listing_id.to_string())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .edition_id
    }

    /// The dedup keys of the stored events of `event_type`.
    async fn event_keys(store: &LibraryStore, event_type: &str) -> Vec<String> {
        let mut keys: Vec<String> = domain_events::Entity::find()
            .filter(domain_events::Column::EventType.eq(event_type))
            .all(&store.db)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.dedup_key)
            .collect();
        keys.sort();
        keys
    }

    /// Records a conflict for `listing_id`.
    async fn conflict(store: &LibraryStore, listing_id: &str, kind: &str, ids: &[&str]) {
        store
            .record_resolution_conflict_for_test(listing_id, kind, ids)
            .await;
    }

    #[tokio::test]
    async fn a_pin_moves_the_listing_and_publishes_both_editions() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ((a, ea), (_, eb)) = two_listings(store).await;
            conflict(store, &a, "strong_identifiers", &[&ea, &eb]).await;
            let linked_before = event_keys(store, "edition_linked").await;

            let outcome = store.pin_listing(&a, &eb, None).await.unwrap();
            assert_eq!(outcome, PinOutcome::Moved { from: ea.clone() }, "{name}");
            assert_eq!(edition_of(store, &a).await, eb, "{name}");
            assert_eq!(
                store.listing_pin(&a).await.unwrap().as_deref(),
                Some(eb.as_str()),
                "{name}"
            );
            let (conflicts, _) = store.list_resolution_conflicts(None, 50).await.unwrap();
            assert!(conflicts.is_empty(), "{name}: the pin settles the conflict");

            let linked = event_keys(store, "edition_linked").await;
            let new: Vec<&String> = linked
                .iter()
                .filter(|k| !linked_before.contains(k))
                .collect();
            assert_eq!(new.len(), 1, "{name}: {linked:?}");
            assert!(
                new[0].starts_with(&format!("edition_linked:{a}:{eb}:")),
                "{name}"
            );
            let updated = event_keys(store, "edition_updated").await;
            assert!(
                updated
                    .iter()
                    .any(|k| k.starts_with(&format!("edition_updated:{ea}:"))),
                "{name}: the edition the listing left is published: {updated:?}"
            );
        }
    }

    #[tokio::test]
    async fn moving_a_listing_back_publishes_again() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ((a, ea), (_, eb)) = two_listings(store).await;

            store.pin_listing(&a, &eb, None).await.unwrap();
            store.pin_listing(&a, &ea, None).await.unwrap();
            store.pin_listing(&a, &eb, None).await.unwrap();
            assert_eq!(edition_of(store, &a).await, eb, "{name}");
            let to_b = event_keys(store, "edition_linked")
                .await
                .into_iter()
                .filter(|k| k.starts_with(&format!("edition_linked:{a}:{eb}:")))
                .count();
            assert_eq!(to_b, 2, "{name}: each move to B is its own event");
        }
    }

    #[tokio::test]
    async fn pinning_where_it_already_is_moves_and_publishes_nothing() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ((a, ea), _) = two_listings(store).await;
            let linked = event_keys(store, "edition_linked").await;
            let updated = event_keys(store, "edition_updated").await;

            let outcome = store.pin_listing(&a, &ea, None).await.unwrap();
            assert_eq!(outcome, PinOutcome::Unchanged, "{name}");
            assert_eq!(
                store.listing_pin(&a).await.unwrap().as_deref(),
                Some(ea.as_str()),
                "{name}"
            );
            assert_eq!(event_keys(store, "edition_linked").await, linked, "{name}");
            assert_eq!(
                event_keys(store, "edition_updated").await,
                updated,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_ids_write_nothing() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ((a, ea), (_, eb)) = two_listings(store).await;

            let outcome = store
                .pin_listing("no-such-listing", &eb, None)
                .await
                .unwrap();
            assert_eq!(outcome, PinOutcome::UnknownListing, "{name}");
            let outcome = store
                .pin_listing(&a, "no-such-edition", None)
                .await
                .unwrap();
            assert_eq!(outcome, PinOutcome::UnknownEdition, "{name}");
            assert_eq!(edition_of(store, &a).await, ea, "{name}");
            assert_eq!(store.listing_pin(&a).await.unwrap(), None, "{name}");

            // The batch itself refuses an edition deleted after the read.
            let statements = pin_statements(&PinWrite {
                listing_id: &a,
                edition_id: "gone",
                from: &ea,
                source: PLUGIN,
                user_id: None,
                revision: "r1",
                now: &now_str(),
            });
            store
                .execute_host_batch(batch::request("pin-gone", statements))
                .await
                .unwrap();
            assert_eq!(edition_of(store, &a).await, ea, "{name}");
            assert_eq!(store.listing_pin(&a).await.unwrap(), None, "{name}");
        }
    }

    #[tokio::test]
    async fn unpinning_keeps_the_listing_where_it_is() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ((a, _), (_, eb)) = two_listings(store).await;
            assert!(
                !store.unpin_listing(&a).await.unwrap(),
                "{name}: nothing to remove"
            );

            store.pin_listing(&a, &eb, None).await.unwrap();
            assert!(store.unpin_listing(&a).await.unwrap(), "{name}");
            assert_eq!(store.listing_pin(&a).await.unwrap(), None, "{name}");
            assert_eq!(edition_of(store, &a).await, eb, "{name}");

            // A later scan of the listing keeps its stored edition.
            store.record_catalog_listing(&listing("A1")).await.unwrap();
            assert_eq!(edition_of(store, &a).await, eb, "{name}");
        }
    }

    #[tokio::test]
    async fn conflicts_page_in_key_order_with_their_listings() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ((a, ea), (b, eb)) = two_listings(store).await;
            conflict(store, &a, "strong_identifiers", &[&eb, &ea]).await;
            conflict(store, &a, "ambiguous_work", &["w1", "w2"]).await;
            conflict(store, &b, "work_kind_mismatch", &["w3"]).await;

            let mut seen = Vec::new();
            let mut after: Option<ConflictCursor> = None;
            loop {
                let (page, next) = store
                    .list_resolution_conflicts(after.as_ref(), 2)
                    .await
                    .unwrap();
                seen.extend(page);
                match next {
                    Some(cursor) => {
                        after = Some(ConflictCursor::decode(&cursor.encode()).unwrap());
                    }
                    None => break,
                }
            }
            let mut expected = vec![
                (a.clone(), "ambiguous_work"),
                (a.clone(), "strong_identifiers"),
                (b.clone(), "work_kind_mismatch"),
            ];
            expected.sort();
            let got: Vec<(String, &str)> = seen
                .iter()
                .map(|c| (c.listing_id.clone(), c.kind.as_str()))
                .collect();
            assert_eq!(got, expected, "{name}");
            let strong = seen
                .iter()
                .find(|c| c.kind == "strong_identifiers")
                .unwrap();
            assert_eq!(strong.title, "Title A1", "{name}");
            assert_eq!(strong.plugin_key, PLUGIN, "{name}");
            assert_eq!(strong.product_id, "A1", "{name}");
            assert_eq!(strong.edition_id, ea, "{name}");
            let mut ids = vec![ea.clone(), eb.clone()];
            ids.sort();
            let mut stored = strong.subject_ids.clone();
            stored.sort();
            assert_eq!(stored, ids, "{name}");
        }
    }

    #[test]
    fn a_bad_cursor_is_refused() {
        assert!(ConflictCursor::decode("not base64 !").is_err());
        assert!(ConflictCursor::decode("e30").is_err());
    }
}
