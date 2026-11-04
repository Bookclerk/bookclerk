//! Store listings resolved into works, expressions and editions.
//!
//! [`LibraryStore::record_listing`] reads the [`resolve::Snapshot`] the
//! resolver needs, runs [`resolve::resolve`] and writes its plan as one
//! guarded batch ([`DbAtomicParams::LinkListing`]). When another writer
//! got there first the batch answers `retry` and the listing is resolved
//! again against the new rows. Credits and series follow in idempotent
//! batches ([`DbAtomicParams::WriteCredits`]); a crash between the two only
//! leaves them for the next scan to write.

use std::collections::BTreeSet;

use bookclerk_resolve as resolve;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{now_str, parse_dt, LibraryStore, NewBook};
use crate::atomic_ops::{atomic_status, DbAtomicParams};
use crate::entities::{
    credits, edition_identifiers, editions, expressions, holdings, resolution_overrides,
    series_entries, store_listings, work_identifiers, works,
};
use crate::error::{LibraryError, Result};
use crate::library_link::{
    ContributorRow, CreditBatchParams, CreditRow, LinkConflictRow, LinkEditionFill, LinkEditionRow,
    LinkEvent, LinkExpressionRow, LinkIdentifierRow, LinkWorkIdentifierRow, LinkWorkRow,
    ListingLinkParams, SeriesEntryRow, SeriesRow,
};
use crate::models::WorkRecord;

/// Times a listing is resolved again after a `retry` before giving up.
const LINK_ATTEMPTS: usize = 3;

/// Most inserts in one credit batch, inside every engine's statement cap
/// with room for the receipt statements.
const CREDIT_BATCH_STATEMENTS: usize = 30;

/// Fresh UUIDs for new rows.
struct UuidIds;

impl resolve::IdSource for UuidIds {
    fn next_id(&mut self, _kind: resolve::EntityKind) -> String {
        Uuid::new_v4().to_string()
    }
}

/// A stored enum spelling the resolver does not know: the row was written
/// by something other than this host.
fn unknown(column: &str, value: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("unknown {column} `{value}` in the library"))
}

/// Parses a stored spelling with `parse`, failing on an unknown one.
fn parse_stored<T>(column: &str, value: &str, parse: fn(&str) -> Option<T>) -> Result<T> {
    parse(value).ok_or_else(|| unknown(column, value))
}

/// The id a contributor or series with `name_key` always gets, so a credit
/// batch never reads before it writes.
fn derived_id(prefix: &str, name_key: &str) -> String {
    let digest = Sha256::digest(name_key.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("{prefix}_{hex}")
}

/// The name a contributor sorts by: the last word first ("Herbert, Frank").
fn sort_name(name: &str) -> String {
    match name.rsplit_once(' ') {
        Some((rest, last)) if !rest.is_empty() => format!("{last}, {rest}"),
        _ => name.to_string(),
    }
}

/// Maps a stored listing row.
fn listing_row(m: store_listings::Model) -> resolve::ListingRow {
    resolve::ListingRow {
        id: m.id,
        key: resolve::ListingKey {
            plugin_key: m.plugin_key,
            marketplace: m.marketplace,
            product_id: m.product_id,
        },
        edition_id: m.edition_id,
    }
}

/// Maps a stored edition identifier row.
fn edition_identifier_row(m: edition_identifiers::Model) -> Result<resolve::EditionIdentifierRow> {
    Ok(resolve::EditionIdentifierRow {
        edition_id: m.edition_id,
        key: resolve::IdentifierKey {
            scheme: parse_stored(
                "identifier scheme",
                &m.scheme,
                resolve::IdentifierScheme::parse,
            )?,
            value: m.value,
            marketplace: m.marketplace,
        },
    })
}

/// Maps a stored work row.
fn work_row(m: &works::Model) -> Result<resolve::WorkRow> {
    Ok(resolve::WorkRow {
        id: m.id.clone(),
        kind: parse_stored("work kind", &m.kind, resolve::WorkKind::parse)?,
        title: m.title.clone(),
        match_key: m.match_key.clone(),
        parent_work_id: m.parent_work_id.clone(),
    })
}

/// Maps a stored expression row.
fn expression_row(m: expressions::Model) -> Result<resolve::ExpressionRow> {
    Ok(resolve::ExpressionRow {
        kind: parse_stored("expression kind", &m.kind, resolve::ExpressionKind::parse)?,
        abridgement: parse_stored("abridgement", &m.abridgement, resolve::Abridgement::parse)?,
        id: m.id,
        work_id: m.work_id,
        language: m.language,
        credit_key: m.credit_key,
    })
}

/// Maps a stored edition row.
fn edition_row(m: editions::Model) -> Result<resolve::EditionRow> {
    Ok(resolve::EditionRow {
        media_kind: parse_stored("media kind", &m.media_kind, resolve::MediaKind::parse)?,
        id: m.id,
        expression_id: m.expression_id,
        title: m.title,
        subtitle: m.subtitle,
        published_at: m.published_at,
        duration_minutes: m.duration_minutes,
    })
}

/// Maps a `works` row for callers outside the resolver.
fn map_work(m: works::Model) -> WorkRecord {
    WorkRecord {
        id: m.id,
        kind: m.kind,
        title: m.title,
        parent_work_id: m.parent_work_id,
        credits: Vec::new(),
        series: Vec::new(),
        description: m.description,
        subjects: m.subjects,
        categories: m.categories,
        cover_url: m.cover_url,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Stored kind of a conflict and the ids it names, as a JSON array.
fn conflict_row(conflict: &resolve::Conflict) -> LinkConflictRow {
    let (kind, mut ids) = match conflict {
        resolve::Conflict::StrongIdentifiers { edition_ids } => {
            ("strong_identifiers", edition_ids.clone())
        }
        resolve::Conflict::WorkIdentifiers { work_ids } => ("work_identifiers", work_ids.clone()),
        resolve::Conflict::AmbiguousWork { work_ids } => ("ambiguous_work", work_ids.clone()),
        resolve::Conflict::WorkKindMismatch { work_id } => {
            ("work_kind_mismatch", vec![work_id.clone()])
        }
    };
    ids.sort();
    ids.dedup();
    LinkConflictRow {
        kind: kind.to_string(),
        subject_ids: serde_json::Value::from(ids).to_string(),
    }
}

/// The outbox row for one resolver event.
fn link_event(listing: &resolve::Listing, event: &resolve::Event) -> LinkEvent {
    let id = Uuid::new_v4().to_string();
    let (account_id, dedup_key, payload, ordering_key) = match event {
        resolve::Event::EditionLinked {
            edition_id,
            listing_id,
        } => (
            String::new(),
            format!("edition_linked:{listing_id}:{edition_id}"),
            serde_json::json!({ "editionId": edition_id, "listingId": listing_id }),
            edition_id.clone(),
        ),
        // A fill only ever adds fields, so each one is its own event.
        resolve::Event::EditionUpdated { edition_id } => (
            String::new(),
            format!("edition_updated:{edition_id}:{id}"),
            serde_json::json!({ "editionId": edition_id }),
            edition_id.clone(),
        ),
        resolve::Event::HoldingAdded { listing_id, holder } => {
            let account = match holder {
                resolve::Holder::Account(account_id) => account_id.clone(),
                resolve::Holder::Node(_) => String::new(),
            };
            (
                account,
                format!("holding_added:{listing_id}:{}", holder.stored()),
                serde_json::json!({ "listingId": listing_id, "holder": holder.stored() }),
                listing_id.clone(),
            )
        }
    };
    LinkEvent {
        id,
        event_type: event.event_type().to_string(),
        account_id,
        source: listing.key.plugin_key.clone(),
        dedup_key,
        payload: payload.to_string(),
        ordering_key,
    }
}

/// The batch parameters for `plan`.
fn link_params(listing: &resolve::Listing, plan: &resolve::Plan) -> ListingLinkParams {
    let gate_strong_keys = if plan.new_edition.is_some() {
        listing
            .edition_identifiers()
            .filter_map(resolve::IdentifierKey::strong_key)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    ListingLinkParams {
        listing_id: plan.listing.id.clone(),
        listing_is_new: plan.listing_is_new,
        plugin_key: listing.key.plugin_key.clone(),
        marketplace: listing.key.marketplace.clone(),
        product_id: listing.key.product_id.clone(),
        title: listing.title.clone(),
        categories: listing.categories.clone(),
        edition_id: plan.listing.edition_id.clone(),
        gate_strong_keys,
        new_work: plan.new_work.as_ref().map(|w| LinkWorkRow {
            id: w.id.clone(),
            kind: w.kind.as_str().to_string(),
            title: w.title.clone(),
            match_key: w.match_key.clone(),
            parent_work_id: w.parent_work_id.clone(),
        }),
        new_expression: plan.new_expression.as_ref().map(|x| LinkExpressionRow {
            id: x.id.clone(),
            work_id: x.work_id.clone(),
            kind: x.kind.as_str().to_string(),
            language: x.language.clone(),
            abridgement: x.abridgement.as_str().to_string(),
            credit_key: x.credit_key.clone(),
        }),
        new_edition: plan.new_edition.as_ref().map(|e| LinkEditionRow {
            id: e.id.clone(),
            expression_id: e.expression_id.clone(),
            media_kind: e.media_kind.as_str().to_string(),
            title: e.title.clone(),
            subtitle: e.subtitle.clone(),
            published_at: e.published_at.clone(),
            duration_minutes: e.duration_minutes,
        }),
        edition_fill: plan.edition_update.as_ref().map(|e| LinkEditionFill {
            subtitle: e.subtitle.clone(),
            published_at: e.published_at.clone(),
            duration_minutes: e.duration_minutes,
        }),
        edition_identifiers: plan
            .edition_identifiers
            .iter()
            .map(|row| LinkIdentifierRow {
                scheme: row.key.scheme.as_str().to_string(),
                value: row.key.value.clone(),
                marketplace: row.key.marketplace.clone(),
                strength: row.key.strength().as_str().to_string(),
                strong_key: row.key.strong_key(),
            })
            .collect(),
        work_id: plan
            .work_identifiers
            .first()
            .map(|row| row.work_id.clone())
            .unwrap_or_default(),
        work_identifiers: plan
            .work_identifiers
            .iter()
            .map(|row| LinkWorkIdentifierRow {
                scheme: row.key.scheme.as_str().to_string(),
                value: row.key.value.clone(),
            })
            .collect(),
        holder: plan.holding.holder.stored(),
        entitlement: plan.holding.entitlement.as_str().to_string(),
        purchased_at: plan.holding.purchased_at.clone(),
        conflicts: plan.conflicts.iter().map(conflict_row).collect(),
        events: plan.events.iter().map(|e| link_event(listing, e)).collect(),
    }
}

/// The credit and series batches for `plan`, each within
/// [`CREDIT_BATCH_STATEMENTS`]; every batch carries the contributors and
/// series its rows reference.
fn credit_batches(plan: &resolve::Plan) -> Vec<CreditBatchParams> {
    let mut batches = Vec::new();
    let mut batch = CreditBatchParams::default();
    for write in &plan.credits {
        if batch.statement_count() + 2 > CREDIT_BATCH_STATEMENTS {
            batches.push(std::mem::take(&mut batch));
        }
        let name_key = write.credit.name_key();
        let contributor_id = derived_id("ctb", &name_key);
        if !batch.contributors.iter().any(|c| c.id == contributor_id) {
            batch.contributors.push(ContributorRow {
                id: contributor_id.clone(),
                name: write.credit.name.clone(),
                sort_name: sort_name(&write.credit.name),
                name_key,
            });
        }
        batch.credits.push(CreditRow {
            target_kind: write.level.as_str().to_string(),
            target_id: write.target_id.clone(),
            contributor_id,
            role: write.credit.role.as_str().to_string(),
            position: i64::from(write.position),
        });
    }
    for write in &plan.series {
        if batch.statement_count() + 2 > CREDIT_BATCH_STATEMENTS {
            batches.push(std::mem::take(&mut batch));
        }
        let name_key = write.entry.name_key();
        let series_id = derived_id("ser", &name_key);
        if !batch.series.iter().any(|s| s.id == series_id) {
            batch.series.push(SeriesRow {
                id: series_id.clone(),
                name: write.entry.name.clone(),
                name_key,
            });
        }
        batch.series_entries.push(SeriesEntryRow {
            series_id,
            work_id: write.work_id.clone(),
            position: write.entry.position.clone(),
            sort_position: write.entry.sort_position(),
        });
    }
    if batch.statement_count() > 0 {
        batches.push(batch);
    }
    batches
}

/// The structured listing a flat library row describes, for paths that
/// only have a [`NewBook`] (imports, seeds, tests).
///
/// Scans that carry a plugin's structured record use that instead.
///
/// # Errors
///
/// Returns [`resolve::ListingError`] when the row fails listing validation.
pub fn listing_from_book(
    book: &NewBook,
) -> std::result::Result<resolve::Listing, resolve::ListingError> {
    let decoded;
    let book = if book.needs_html_entity_decode() {
        let mut copy = book.clone();
        copy.decode_html_entities();
        decoded = copy;
        &decoded
    } else {
        book
    };
    let episode = crate::models::is_episode(&book.content_kind);
    let show = crate::models::is_podcast_parent(&book.content_kind);
    let (work_kind, expression_kind, media_kind) = if episode {
        (
            resolve::WorkKind::PodcastEpisode,
            resolve::ExpressionKind::PodcastAudio,
            resolve::MediaKind::PodcastAudio,
        )
    } else if show {
        (
            resolve::WorkKind::PodcastShow,
            resolve::ExpressionKind::PodcastAudio,
            resolve::MediaKind::PodcastAudio,
        )
    } else {
        (
            resolve::WorkKind::Book,
            resolve::ExpressionKind::Narration,
            resolve::MediaKind::Audiobook,
        )
    };
    let mut credits = Vec::new();
    for (names, role) in [
        (&book.authors, resolve::CreditRole::Author),
        (&book.narrators, resolve::CreditRole::Narrator),
        (&book.publisher, resolve::CreditRole::Publisher),
    ] {
        let Some(names) = names.as_deref() else {
            continue;
        };
        let split = if role == resolve::CreditRole::Publisher {
            vec![names.to_string()]
        } else {
            resolve::names::split_display_names(names)
        };
        credits.extend(split.into_iter().map(|name| (name, role, None)));
    }
    let mut identifiers = Vec::new();
    if let Some(asin) = book.asin.as_deref().filter(|a| !a.trim().is_empty()) {
        identifiers.push((
            resolve::IdentifierScheme::Asin,
            asin.to_string(),
            Some(book.marketplace.clone()),
        ));
    }
    if let Some(isbn) = book.isbn.as_deref().filter(|i| !i.trim().is_empty()) {
        identifiers.push((resolve::IdentifierScheme::Isbn, isbn.to_string(), None));
    }
    let draft = resolve::ListingDraft {
        plugin_key: book.plugin_key.clone(),
        marketplace: Some(book.marketplace.clone()),
        product_id: book.product_id.clone(),
        work_kind: Some(work_kind),
        expression_kind: Some(expression_kind),
        media_kind: Some(media_kind),
        language: None,
        abridgement: Some(if book.is_abridged {
            resolve::Abridgement::Abridged
        } else {
            resolve::Abridgement::Unknown
        }),
        title: book.title.clone(),
        subtitle: book.subtitle.clone(),
        published_at: book.published_at.map(|t| t.to_rfc3339()),
        duration_minutes: book.length_minutes,
        categories: book.categories.clone(),
        identifiers,
        credits,
        series: book
            .series
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| (s.clone(), book.series_index.clone()))
            .collect(),
        parent_product_id: None,
    };
    resolve::Listing::from_draft(&draft)
}

/// The holding a flat library row describes.
#[must_use]
pub fn holding_from_book(book: &NewBook) -> resolve::HoldingDraft {
    resolve::HoldingDraft {
        holder: resolve::Holder::Account(book.account_id.clone()),
        entitlement: resolve::Entitlement::Purchase,
        purchased_at: book.purchased_at.map(|t| t.to_rfc3339()),
    }
}

impl LibraryStore {
    /// Resolves `listing` into the library and records `holding` of it.
    ///
    /// # Arguments
    ///
    /// * `listing` - The validated listing a storefront reported.
    /// * `holding` - Who holds it, and how.
    ///
    /// # Returns
    ///
    /// The plan that was written (a no-op plan when nothing changed).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read or write fails, when the stored
    /// rows are inconsistent, or when other writers kept changing the rows
    /// for three attempts.
    pub async fn record_listing(
        &self,
        listing: &resolve::Listing,
        holding: &resolve::HoldingDraft,
    ) -> Result<resolve::Plan> {
        for _ in 0..LINK_ATTEMPTS {
            let snapshot = self.listing_snapshot(listing, &holding.holder).await?;
            if let Some(missing) = snapshot.is_closed() {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "library rows for listing {} reference a missing {missing}",
                    listing.key.product_id
                )));
            }
            let plan = resolve::resolve(listing, holding, &snapshot, &mut UuidIds);
            if !plan.is_noop() || !plan.conflicts.is_empty() {
                let params = link_params(listing, &plan);
                if self.run_link_listing(params).await? != atomic_status::OK {
                    continue;
                }
            }
            for batch in credit_batches(&plan) {
                self.run_write_credits(batch).await?;
            }
            return Ok(plan);
        }
        Err(LibraryError::Other(anyhow::anyhow!(
            "listing {} kept changing under concurrent writers",
            listing.key.product_id
        )))
    }

    /// Runs one link batch and returns its status (`ok` or `retry`).
    async fn run_link_listing(&self, params: ListingLinkParams) -> Result<String> {
        if let Some(atomic) = &self.atomic {
            return atomic.link_listing(params).await;
        }
        let op = DbAtomicParams::LinkListing(Box::new(params));
        let result =
            crate::db_atomic::execute_named_atomic(&self.db, &Uuid::new_v4().to_string(), &op)
                .await?;
        Ok(result.status)
    }

    /// Runs one credit batch.
    async fn run_write_credits(&self, params: CreditBatchParams) -> Result<()> {
        if let Some(atomic) = &self.atomic {
            return atomic.write_credits(params).await;
        }
        let op = DbAtomicParams::WriteCredits(Box::new(params));
        let result =
            crate::db_atomic::execute_named_atomic(&self.db, &Uuid::new_v4().to_string(), &op)
                .await?;
        if result.status == atomic_status::OK {
            Ok(())
        } else {
            Err(LibraryError::Other(anyhow::anyhow!(
                "credit batch answered `{}`",
                result.status
            )))
        }
    }

    /// Reads the stored rows that bear on `listing`, as the resolver's
    /// [`resolve::Snapshot`] contract describes.
    async fn listing_snapshot(
        &self,
        listing: &resolve::Listing,
        holder: &resolve::Holder,
    ) -> Result<resolve::Snapshot> {
        let db = &self.db;
        let find_listing = |key: &resolve::ListingKey| {
            store_listings::Entity::find()
                .filter(store_listings::Column::PluginKey.eq(key.plugin_key.as_str()))
                .filter(store_listings::Column::Marketplace.eq(key.marketplace.as_str()))
                .filter(store_listings::Column::ProductId.eq(key.product_id.as_str()))
        };
        let stored = find_listing(&listing.key)
            .one(db)
            .await
            .map_err(LibraryError::Orm)?
            .map(listing_row);
        let parent = match listing.parent_key() {
            Some(key) => find_listing(&key)
                .one(db)
                .await
                .map_err(LibraryError::Orm)?
                .map(listing_row),
            None => None,
        };

        let mut overrides = Vec::new();
        if let Some(row) = &stored {
            let pins = resolution_overrides::Entity::find()
                .filter(resolution_overrides::Column::SubjectKind.eq("listing"))
                .filter(resolution_overrides::Column::SubjectId.eq(row.id.as_str()))
                .filter(resolution_overrides::Column::Action.eq("pin_edition"))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?;
            overrides.extend(pins.into_iter().filter_map(|m| {
                m.target_id.map(|edition_id| resolve::Override::PinListing {
                    listing: listing.key.clone(),
                    edition_id,
                })
            }));
        }

        // Identifier rows sharing one of the listing's edition identifiers.
        let keys: Vec<&resolve::IdentifierKey> = listing.edition_identifiers().collect();
        let mut identifier_rows = BTreeSet::new();
        if !keys.is_empty() {
            let mut any = Condition::any();
            for key in &keys {
                any = any.add(
                    Condition::all()
                        .add(edition_identifiers::Column::Scheme.eq(key.scheme.as_str()))
                        .add(edition_identifiers::Column::Value.eq(key.value.as_str()))
                        .add(edition_identifiers::Column::Marketplace.eq(key.marketplace.as_str())),
                );
            }
            for m in edition_identifiers::Entity::find()
                .filter(any)
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                identifier_rows.insert(edition_identifier_row(m)?);
            }
        }

        let mut edition_ids: BTreeSet<String> = BTreeSet::new();
        edition_ids.extend(stored.iter().map(|l| l.edition_id.clone()));
        edition_ids.extend(parent.iter().map(|l| l.edition_id.clone()));
        edition_ids.extend(overrides.iter().filter_map(|o| match o {
            resolve::Override::PinListing { edition_id, .. } => Some(edition_id.clone()),
            resolve::Override::IsolateWork { .. } => None,
        }));
        edition_ids.extend(identifier_rows.iter().map(|r| r.edition_id.clone()));

        let mut editions_found = Vec::new();
        if !edition_ids.is_empty() {
            for m in editions::Entity::find()
                .filter(editions::Column::Id.is_in(edition_ids.iter().cloned()))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                editions_found.push(edition_row(m)?);
            }
            for m in edition_identifiers::Entity::find()
                .filter(edition_identifiers::Column::EditionId.is_in(edition_ids.iter().cloned()))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                identifier_rows.insert(edition_identifier_row(m)?);
            }
        }

        let mut work_identifier_rows = Vec::new();
        let work_keys: Vec<&resolve::IdentifierKey> = listing.work_identifiers().collect();
        if !work_keys.is_empty() {
            let mut any = Condition::any();
            for key in &work_keys {
                any = any.add(
                    Condition::all()
                        .add(work_identifiers::Column::Scheme.eq(key.scheme.as_str()))
                        .add(work_identifiers::Column::Value.eq(key.value.as_str())),
                );
            }
            for m in work_identifiers::Entity::find()
                .filter(any)
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                work_identifier_rows.push(resolve::WorkIdentifierRow {
                    key: resolve::IdentifierKey {
                        scheme: parse_stored(
                            "identifier scheme",
                            &m.scheme,
                            resolve::IdentifierScheme::parse,
                        )?,
                        value: m.value,
                        marketplace: String::new(),
                    },
                    work_id: m.work_id,
                });
            }
        }

        let expression_ids: BTreeSet<String> = editions_found
            .iter()
            .map(|e| e.expression_id.clone())
            .collect();
        let mut expressions_found = Vec::new();
        if !expression_ids.is_empty() {
            for m in expressions::Entity::find()
                .filter(expressions::Column::Id.is_in(expression_ids.iter().cloned()))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                expressions_found.push(expression_row(m)?);
            }
        }

        let mut work_ids: BTreeSet<String> = expressions_found
            .iter()
            .map(|x| x.work_id.clone())
            .collect();
        work_ids.extend(work_identifier_rows.iter().map(|r| r.work_id.clone()));
        let match_key = listing.work_match_key();
        if !match_key.is_empty() {
            let matched = works::Entity::find()
                .filter(works::Column::MatchKey.eq(match_key.as_str()))
                .filter(works::Column::Kind.eq(listing.work_kind.as_str()))
                .select_only()
                .column(works::Column::Id)
                .into_tuple::<String>()
                .all(db)
                .await
                .map_err(LibraryError::Orm)?;
            work_ids.extend(matched);
        }

        let mut works_found = Vec::new();
        if !work_ids.is_empty() {
            for m in works::Entity::find()
                .filter(works::Column::Id.is_in(work_ids.iter().cloned()))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                works_found.push(work_row(&m)?);
            }
            for m in expressions::Entity::find()
                .filter(expressions::Column::WorkId.is_in(work_ids.iter().cloned()))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                if !expressions_found.iter().any(|x| x.id == m.id) {
                    expressions_found.push(expression_row(m)?);
                }
            }
            let isolated = resolution_overrides::Entity::find()
                .filter(resolution_overrides::Column::SubjectKind.eq("work"))
                .filter(resolution_overrides::Column::Action.eq("isolate"))
                .filter(resolution_overrides::Column::SubjectId.is_in(work_ids.iter().cloned()))
                .all(db)
                .await
                .map_err(LibraryError::Orm)?;
            overrides.extend(
                isolated
                    .into_iter()
                    .map(|m| resolve::Override::IsolateWork {
                        work_id: m.subject_id,
                    }),
            );
        }

        let holding = match &stored {
            Some(row) => holdings::Entity::find_by_id((row.id.clone(), holder.stored()))
                .one(db)
                .await
                .map_err(LibraryError::Orm)?
                .map(|m| -> Result<resolve::HoldingRow> {
                    Ok(resolve::HoldingRow {
                        holder: resolve::Holder::parse(&m.holder)
                            .ok_or_else(|| unknown("holder", &m.holder))?,
                        entitlement: parse_stored(
                            "entitlement",
                            &m.entitlement,
                            resolve::Entitlement::parse,
                        )?,
                        listing_id: m.listing_id,
                        purchased_at: m.purchased_at,
                    })
                })
                .transpose()?,
            None => None,
        };

        let all_expression_ids: BTreeSet<String> =
            expressions_found.iter().map(|x| x.id.clone()).collect();
        let mut credited = BTreeSet::new();
        for (level, ids) in [
            (resolve::Level::Work, &work_ids),
            (resolve::Level::Expression, &all_expression_ids),
            (resolve::Level::Edition, &edition_ids),
        ] {
            if ids.is_empty() {
                continue;
            }
            let targets = credits::Entity::find()
                .filter(credits::Column::TargetKind.eq(level.as_str()))
                .filter(credits::Column::TargetId.is_in(ids.iter().cloned()))
                .select_only()
                .column(credits::Column::TargetId)
                .distinct()
                .into_tuple::<String>()
                .all(db)
                .await
                .map_err(LibraryError::Orm)?;
            credited.extend(targets.into_iter().map(|id| (level, id)));
        }
        let works_with_series = if work_ids.is_empty() {
            Vec::new()
        } else {
            series_entries::Entity::find()
                .filter(series_entries::Column::WorkId.is_in(work_ids.iter().cloned()))
                .select_only()
                .column(series_entries::Column::WorkId)
                .distinct()
                .into_tuple::<String>()
                .all(db)
                .await
                .map_err(LibraryError::Orm)?
        };

        Ok(resolve::Snapshot {
            listing: stored,
            parent_listing: parent,
            edition_identifiers: identifier_rows.into_iter().collect(),
            work_identifiers: work_identifier_rows,
            editions: editions_found,
            expressions: expressions_found,
            works: works_found,
            overrides,
            holding,
            credited: credited.into_iter().collect(),
            works_with_series,
        })
    }

    /// Loads a work by id, with its work-level credits and series.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn get_work(&self, id: &str) -> Result<Option<WorkRecord>> {
        let Some(work) = works::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let mut works = vec![map_work(work)];
        self.fill_work_names(&mut works).await?;
        Ok(works.pop())
    }

    /// Lists every work by title, with its work-level credits and series.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn list_works(&self) -> Result<Vec<WorkRecord>> {
        let mut works: Vec<WorkRecord> = works::Entity::find()
            .order_by_asc(works::Column::Title)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_work)
            .collect();
        self.fill_work_names(&mut works).await?;
        Ok(works)
    }

    /// Fills each work's credited names and series names.
    async fn fill_work_names(&self, works: &mut [WorkRecord]) -> Result<()> {
        use crate::entities::{contributors, series};
        use std::collections::BTreeMap;
        if works.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = works.iter().map(|w| w.id.clone()).collect();
        let mut work_credits = Vec::new();
        let mut entries = Vec::new();
        for chunk in ids.chunks(usize::try_from(self.max_binds).unwrap_or(100).clamp(1, 90)) {
            work_credits.extend(
                credits::Entity::find()
                    .filter(credits::Column::TargetKind.eq(resolve::Level::Work.as_str()))
                    .filter(credits::Column::TargetId.is_in(chunk.iter().cloned()))
                    .order_by_asc(credits::Column::Position)
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?,
            );
            entries.extend(
                series_entries::Entity::find()
                    .filter(series_entries::Column::WorkId.is_in(chunk.iter().cloned()))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?,
            );
        }
        let contributor_ids: BTreeSet<String> = work_credits
            .iter()
            .map(|c| c.contributor_id.clone())
            .collect();
        let series_ids: BTreeSet<String> = entries.iter().map(|e| e.series_id.clone()).collect();
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        let contributor_ids: Vec<String> = contributor_ids.into_iter().collect();
        for chunk in contributor_ids.chunks(90) {
            names.extend(
                contributors::Entity::find()
                    .filter(contributors::Column::Id.is_in(chunk.iter().cloned()))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .into_iter()
                    .map(|c| (c.id, c.name)),
            );
        }
        let series_ids: Vec<String> = series_ids.into_iter().collect();
        for chunk in series_ids.chunks(90) {
            names.extend(
                series::Entity::find()
                    .filter(series::Column::Id.is_in(chunk.iter().cloned()))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .into_iter()
                    .map(|s| (s.id, s.name)),
            );
        }
        for work in works.iter_mut() {
            work.credits = work_credits
                .iter()
                .filter(|c| c.target_id == work.id)
                .filter_map(|c| names.get(&c.contributor_id).cloned())
                .collect();
            work.series = entries
                .iter()
                .filter(|e| e.work_id == work.id)
                .filter_map(|e| names.get(&e.series_id).cloned())
                .collect();
        }
        Ok(())
    }

    /// The work a library title's listing resolves to, if it has been
    /// resolved.
    ///
    /// # Arguments
    ///
    /// * `book_uuid` - Library `books.uuid`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn work_id_for_book(&self, book_uuid: &str) -> Result<Option<String>> {
        let Some(book) = crate::entities::books::Entity::find()
            .filter(crate::entities::books::Column::Uuid.eq(book_uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let marketplace = resolve::stored_marketplace(&book.marketplace);
        let Some(listing) = store_listings::Entity::find()
            .filter(store_listings::Column::PluginKey.eq(book.plugin_key.as_str()))
            .filter(store_listings::Column::Marketplace.eq(marketplace.as_str()))
            .filter(store_listings::Column::ProductId.eq(book.product_id.as_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        self.work_id_for_edition(&listing.edition_id).await
    }

    /// The work an edition belongs to.
    async fn work_id_for_edition(&self, edition_id: &str) -> Result<Option<String>> {
        let Some(edition) = editions::Entity::find_by_id(edition_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(expressions::Entity::find_by_id(edition.expression_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|x| x.work_id))
    }

    /// Library title UUIDs whose listings resolve to `work_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn book_uuids_for_work(&self, work_id: &str) -> Result<Vec<String>> {
        use crate::entities::books;
        let expression_ids: Vec<String> = expressions::Entity::find()
            .filter(expressions::Column::WorkId.eq(work_id))
            .select_only()
            .column(expressions::Column::Id)
            .into_tuple::<String>()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if expression_ids.is_empty() {
            return Ok(Vec::new());
        }
        let edition_ids: Vec<String> = editions::Entity::find()
            .filter(editions::Column::ExpressionId.is_in(expression_ids))
            .select_only()
            .column(editions::Column::Id)
            .into_tuple::<String>()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if edition_ids.is_empty() {
            return Ok(Vec::new());
        }
        let listings = store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.is_in(edition_ids))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut uuids = BTreeSet::new();
        for listing in listings {
            let found: Vec<String> = books::Entity::find()
                .filter(books::Column::PluginKey.eq(listing.plugin_key.as_str()))
                .filter(books::Column::ProductId.eq(listing.product_id.as_str()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .into_iter()
                .filter(|b| resolve::stored_marketplace(&b.marketplace) == listing.marketplace)
                .map(|b| b.uuid)
                .collect();
            uuids.extend(found);
        }
        Ok(uuids.into_iter().collect())
    }

    /// Fills a work's enrichment fields; `None` keeps the stored value.
    ///
    /// # Arguments
    ///
    /// * `work_id` - The work.
    /// * `enrichment` - Description, subjects and cover found by enrichment.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] when the work is gone, or
    /// [`LibraryError::Orm`] when the write fails.
    pub async fn set_work_enrichment(
        &self,
        work_id: &str,
        enrichment: &WorkEnrichment,
    ) -> Result<()> {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let Some(existing) = works::Entity::find_by_id(work_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Err(LibraryError::NotFound(format!("work {work_id}")));
        };
        let mut am: works::ActiveModel = existing.clone().into();
        am.description = Set(enrichment.description.clone().or(existing.description));
        am.subjects = Set(enrichment.subjects.clone().or(existing.subjects));
        am.cover_url = Set(enrichment.cover_url.clone().or(existing.cover_url));
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Attaches a work-level identifier enrichment found (an OpenLibrary
    /// work id); a value another work already holds is left there.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn add_work_identifier(
        &self,
        work_id: &str,
        scheme: resolve::IdentifierScheme,
        raw: &str,
    ) -> Result<()> {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        if scheme.level() != resolve::Level::Work {
            return Ok(());
        }
        let Some(key) = resolve::IdentifierKey::new(scheme, raw, None, "") else {
            return Ok(());
        };
        let held = work_identifiers::Entity::find_by_id((
            key.scheme.as_str().to_string(),
            key.value.clone(),
        ))
        .one(&self.db)
        .await
        .map_err(LibraryError::Orm)?;
        if held.is_some() {
            return Ok(());
        }
        let am = work_identifiers::ActiveModel {
            scheme: Set(key.scheme.as_str().to_string()),
            value: Set(key.value),
            work_id: Set(work_id.to_string()),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }
}

/// Enrichment fields for [`LibraryStore::set_work_enrichment`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkEnrichment {
    /// Blurb / synopsis text.
    pub description: Option<String>,
    /// Subject tags (serialized).
    pub subjects: Option<String>,
    /// HTTPS cover URL.
    pub cover_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_are_stable_and_prefixed() {
        assert_eq!(
            derived_id("ctb", "frank herbert"),
            derived_id("ctb", "frank herbert")
        );
        assert!(derived_id("ser", "dune").starts_with("ser_"));
        assert_ne!(derived_id("ctb", "a"), derived_id("ctb", "b"));
    }

    #[test]
    fn sort_names_put_the_last_word_first() {
        assert_eq!(sort_name("Frank Herbert"), "Herbert, Frank");
        assert_eq!(sort_name("Plato"), "Plato");
    }

    #[test]
    fn conflicts_store_sorted_json_ids() {
        let row = conflict_row(&resolve::Conflict::StrongIdentifiers {
            edition_ids: vec!["b".into(), "a".into(), "a".into()],
        });
        assert_eq!(row.kind, "strong_identifiers");
        assert_eq!(row.subject_ids, r#"["a","b"]"#);
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::entities::{credits, holdings, resolution_conflicts, series_entries};
    use crate::{InProcessSqliteAtomic, LibraryStore};
    use sea_orm::PaginatorTrait;
    use std::sync::Arc;

    /// A store over a fresh in-memory library.
    async fn store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    /// A Dune audiobook draft sold by `plugin` as `product`.
    fn dune(plugin: &str, product: &str) -> resolve::ListingDraft {
        resolve::ListingDraft {
            plugin_key: plugin.into(),
            marketplace: Some("us".into()),
            product_id: product.into(),
            work_kind: Some(resolve::WorkKind::Book),
            expression_kind: Some(resolve::ExpressionKind::Narration),
            media_kind: Some(resolve::MediaKind::Audiobook),
            language: Some("en".into()),
            abridgement: Some(resolve::Abridgement::Unabridged),
            title: "Dune".into(),
            subtitle: None,
            published_at: None,
            duration_minutes: Some(1260),
            categories: None,
            identifiers: vec![(
                resolve::IdentifierScheme::Isbn,
                "9780593288368".into(),
                None,
            )],
            credits: vec![
                ("Frank Herbert".into(), resolve::CreditRole::Author, None),
                ("Scott Brick".into(), resolve::CreditRole::Narrator, None),
            ],
            series: vec![("Dune".into(), Some("1".into()))],
            parent_product_id: None,
        }
    }

    /// The holding of account `id`.
    fn account(id: &str) -> resolve::HoldingDraft {
        resolve::HoldingDraft {
            holder: resolve::Holder::Account(id.into()),
            entitlement: resolve::Entitlement::Purchase,
            purchased_at: None,
        }
    }

    #[tokio::test]
    async fn two_accounts_and_two_stores_share_one_edition() {
        let store = store().await;
        let audible = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        let libro = resolve::Listing::from_draft(&dune("libro", "L1")).unwrap();

        let first = store
            .record_listing(&audible, &account("a1"))
            .await
            .unwrap();
        assert_eq!(first.link, resolve::LinkReason::New);
        let second = store
            .record_listing(&audible, &account("a2"))
            .await
            .unwrap();
        assert_eq!(second.link, resolve::LinkReason::Listing);
        let third = store.record_listing(&libro, &account("a1")).await.unwrap();
        assert_eq!(third.link, resolve::LinkReason::WeakIdentifier);
        assert_eq!(third.listing.edition_id, first.listing.edition_id);

        let again = store.record_listing(&libro, &account("a1")).await.unwrap();
        assert!(again.is_noop(), "{again:?}");

        let db = &store.db;
        assert_eq!(holdings::Entity::find().count(db).await.unwrap(), 3);
        assert_eq!(works::Entity::find().count(db).await.unwrap(), 1);
        // Author at work, narrator at expression; written once each.
        assert_eq!(credits::Entity::find().count(db).await.unwrap(), 2);
        assert_eq!(series_entries::Entity::find().count(db).await.unwrap(), 1);
        assert_eq!(store.list_works().await.unwrap()[0].title, "Dune");
    }

    #[tokio::test]
    async fn strong_identifier_clash_is_recorded_not_merged() {
        let store = store().await;
        let mut a = dune("audible", "B001");
        a.identifiers = vec![(resolve::IdentifierScheme::Asin, "B001".into(), None)];
        let mut b = dune("audible", "B002");
        b.identifiers = vec![(resolve::IdentifierScheme::Asin, "B002".into(), None)];
        let mut both = dune("libro", "L1");
        both.identifiers = vec![
            (resolve::IdentifierScheme::Asin, "B001".into(), None),
            (
                resolve::IdentifierScheme::OpenLibraryEdition,
                "OL1M".into(),
                None,
            ),
        ];
        let mut ol = dune("audible", "B003");
        ol.identifiers = vec![(
            resolve::IdentifierScheme::OpenLibraryEdition,
            "OL1M".into(),
            None,
        )];
        ol.media_kind = Some(resolve::MediaKind::Ebook);

        let a = store
            .record_listing(&resolve::Listing::from_draft(&a).unwrap(), &account("a1"))
            .await
            .unwrap();
        let ol = store
            .record_listing(&resolve::Listing::from_draft(&ol).unwrap(), &account("a1"))
            .await
            .unwrap();
        store
            .record_listing(&resolve::Listing::from_draft(&b).unwrap(), &account("a1"))
            .await
            .unwrap();
        let joined = store
            .record_listing(
                &resolve::Listing::from_draft(&both).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        assert_ne!(a.listing.edition_id, ol.listing.edition_id);
        assert!(!joined.conflicts.is_empty());
        assert_eq!(
            resolution_conflicts::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn books_reach_their_work_both_ways() {
        let store = store().await;
        store
            .upsert_account("audible", "a1", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00X", "a1", "US", "Dune");
        book.authors = Some("Frank Herbert".into());
        let record = store.upsert_book(&book).await.unwrap();
        let work_id = store.work_id_for_book(&record.uuid).await.unwrap().unwrap();
        assert_eq!(
            store.book_uuids_for_work(&work_id).await.unwrap(),
            vec![record.uuid.clone()]
        );
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some("Spice.".into()),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        store
            .add_work_identifier(&work_id, resolve::IdentifierScheme::OpenLibraryWork, "OL1W")
            .await
            .unwrap();
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.description.as_deref(), Some("Spice."));
        assert_eq!(work.kind, "book");
        assert!(store.work_id_for_book("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_guest_backend_path_writes_the_same_rows() {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let store = LibraryStore::from_connection(db.clone())
            .with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
        let listing = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        let plan = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert_eq!(plan.link, resolve::LinkReason::New);
        assert!(store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap()
            .is_noop());
    }

    #[test]
    fn large_credit_lists_split_into_bounded_batches() {
        let mut draft = dune("audible", "B001");
        draft.credits = (0..40)
            .map(|i| (format!("Cast Member {i}"), resolve::CreditRole::Cast, None))
            .collect();
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let plan = resolve::resolve(
            &listing,
            &account("a1"),
            &resolve::Snapshot::default(),
            &mut UuidIds,
        );
        let batches = credit_batches(&plan);
        assert!(batches.len() > 1);
        assert!(batches
            .iter()
            .all(|b| b.statement_count() <= CREDIT_BATCH_STATEMENTS));
        let credits: usize = batches.iter().map(|b| b.credits.len()).sum();
        assert_eq!(credits, 40);
    }

    #[test]
    fn flat_rows_become_listings() {
        let mut book = NewBook::minimal("B00X", "a1", "", "Dune &amp; More");
        book.isbn = Some("9780593288368".into());
        book.narrators = Some("Scott Brick, Simon Vance".into());
        book.content_kind = "episode".into();
        let listing = listing_from_book(&book).unwrap();
        assert_eq!(listing.title, "Dune & More");
        assert_eq!(listing.key.marketplace, "us");
        assert_eq!(listing.work_kind, resolve::WorkKind::PodcastEpisode);
        assert_eq!(listing.identifiers.len(), 2);
        assert_eq!(listing.credits.len(), 2);
        assert_eq!(holding_from_book(&book).holder.stored(), "account:a1");
    }
}
