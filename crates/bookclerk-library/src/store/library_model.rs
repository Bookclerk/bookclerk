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
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::listing_reads::{
    edition_identifier_key, work_identifier_key, ListingPool, SnapshotReader,
};
use super::{now_str, parse_dt, LibraryStore, NewBook};
use crate::atomic_ops::{atomic_status, DbAtomicParams};
use crate::entities::{
    credits, edition_identifiers, editions, embeddings, expressions, series_entries,
    store_listings, work_identifiers, works,
};
use crate::error::{LibraryError, Result};
use crate::library_link::{
    ContributorRow, CreditBatchParams, CreditRow, LinkConflictRow, LinkEditionFill, LinkEditionRow,
    LinkEvent, LinkExpressionFill, LinkExpressionRow, LinkHoldingRow, LinkIdentifierRow,
    LinkWorkIdentifierRow, LinkWorkMatchKey, LinkWorkParent, LinkWorkRow, ListingLinkParams,
    SeriesEntryRow, SeriesRow,
};
use crate::models::WorkRecord;

/// Most ids bound in one `IN (...)` lookup, under D1's bind cap.
const LOOKUP_CHUNK: usize = 90;

/// Binds a lookup keeps free for its predicates besides the `IN (...)`
/// list (at most a kind, a role or action, and a position today).
const LOOKUP_RESERVED_BINDS: usize = 8;

/// Ids bound per `IN (...)` lookup on a backend that accepts `max_binds`
/// binds per statement: [`LOOKUP_CHUNK`], or fewer on a smaller backend,
/// leaving [`LOOKUP_RESERVED_BINDS`] for the other predicates.
fn lookup_chunk(max_binds: u32) -> usize {
    usize::try_from(max_binds)
        .unwrap_or(LOOKUP_CHUNK)
        .saturating_sub(LOOKUP_RESERVED_BINDS)
        .clamp(1, LOOKUP_CHUNK)
}

/// Rows of `E` that `query` selects for `ids`, read in chunks no wider than
/// [`LibraryStore::lookup_chunk`] or the backend's `maxResultRows`, and
/// narrowed further until each statement (SQL plus binds) fits the
/// backend's `maxBinds`, `maxPayloadBytes` and `maxRequestBytes`, so a
/// lookup of long ids (UUIDs on D1) never fails where reading them one by
/// one would fit. A chunk whose reply is refused for its size is read again
/// in halves.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a read fails.
pub(super) async fn find_chunked<E, F>(
    store: &LibraryStore,
    ids: &[String],
    query: F,
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
    F: Fn(&[String]) -> sea_orm::Select<E>,
{
    find_chunked_by(store, ids, 1, query).await
}

/// [`find_chunked`] over composite keys that each bind `binds_per_key`
/// values: chunks start [`LibraryStore::lookup_chunk`]` / binds_per_key`
/// keys wide and narrow until the statement fits.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a read fails.
pub(super) async fn find_chunked_by<K, E, F>(
    store: &LibraryStore,
    keys: &[K],
    binds_per_key: usize,
    query: F,
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
    F: Fn(&[K]) -> sea_orm::Select<E>,
{
    let fits = |part: &[K]| {
        crate::control_plane::sea_exec(&query(part))
            .map(|stmt| store.admits_read(&stmt))
            .unwrap_or(true)
    };
    let caps = store.db_capabilities();
    let mut width = (store.lookup_chunk() / binds_per_key.max(1)).max(1);
    if caps.max_result_rows > 0 {
        // Lookups by a unique key return at most one row per key, so a
        // chunk no wider than the row cap fits it; other lookups narrow on
        // a refused reply below.
        width = width.min(usize::try_from(caps.max_result_rows).unwrap_or(usize::MAX));
    }
    read_in_chunks(keys, width, fits, |part| query(part).all(&store.db)).await
}

/// Reads `keys` in chunks of at most `width`, halving a chunk until `fits`
/// accepts it and again when its read is refused as over a negotiated limit
/// (a reply too large for `maxResultRows` / `maxResultBytes`). Later chunks
/// start no wider than the last that was read.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a read fails for another reason, or a
/// single key's read is still over a limit.
async fn read_in_chunks<K, T, Fut>(
    keys: &[K],
    width: usize,
    fits: impl Fn(&[K]) -> bool,
    read: impl Fn(&[K]) -> Fut,
) -> Result<Vec<T>>
where
    Fut: std::future::Future<Output = std::result::Result<Vec<T>, sea_orm::DbErr>>,
{
    let mut out = Vec::new();
    let mut width = width.max(1);
    let mut rest = keys;
    while !rest.is_empty() {
        let mut n = width.min(rest.len());
        while n > 1 && !fits(&rest[..n]) {
            n /= 2;
        }
        let (part, tail) = rest.split_at(n);
        match read(part).await {
            Ok(rows) => {
                out.extend(rows);
                width = n;
                rest = tail;
            }
            Err(err)
                if n > 1
                    && bookclerk_plugin_abi::LimitExceeded::from_message(&err.to_string())
                        .is_some() =>
            {
                width = n / 2;
            }
            Err(err) => return Err(LibraryError::Orm(err)),
        }
    }
    Ok(out)
}

/// Times a listing is resolved again after a `retry` before giving up.
const LINK_ATTEMPTS: usize = 3;

/// Most inserts in one credit batch, inside every engine's statement cap
/// with room for the receipt statements.
const CREDIT_BATCH_STATEMENTS: usize = 30;

// One listing's series (a series row and an entry each) fit in one batch.
const _: () = assert!(2 * resolve::listing::MAX_SERIES <= CREDIT_BATCH_STATEMENTS);

/// Fresh UUIDs for new rows; an edition's is derived from the listing that
/// first named it ([`edition_id_for_listing`]) unless `random_editions`.
#[derive(Default)]
struct UuidIds {
    /// Mint random edition ids too: the derived id is already taken (the
    /// listing's first edition still holds other listings after the
    /// listing was moved off it).
    random_editions: bool,
}

impl resolve::IdSource for UuidIds {
    fn next_id(&mut self, _kind: resolve::EntityKind) -> String {
        Uuid::new_v4().to_string()
    }

    fn edition_id(&mut self, key: &resolve::listing::ListingKey) -> String {
        if self.random_editions {
            self.next_id(resolve::EntityKind::Edition)
        } else {
            edition_id_for_listing(key)
        }
    }
}

/// Namespace of the name-based (version 5) UUIDs that edition ids are.
const EDITION_ID_NAMESPACE: Uuid = Uuid::from_u128(0x6b1f_4e0a_93c2_5d7e_8a41_0c9b_2f63_d815);

/// The id of an edition first seen through the store listing `key`: a
/// version 5 UUID over its plugin key, marketplace and product id. Two
/// nodes (or one library rebuilt from a fresh scan) that resolve the same
/// listing first give its edition the same id, and so the same
/// `<library id>` in file names.
#[must_use]
pub fn edition_id_for_listing(key: &resolve::listing::ListingKey) -> String {
    let name = format!(
        "edition\0{}\0{}\0{}",
        key.plugin_key, key.marketplace, key.product_id
    );
    Uuid::new_v5(&EDITION_ID_NAMESPACE, name.as_bytes()).to_string()
}

/// Lower-case Crockford base32 digits, for [`library_id`].
const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The `<library id>` of edition `edition_id`: its first 40 bits as 8
/// lower-case Crockford base32 characters (no `i`, `l`, `o` or `u`). An id
/// that is not a UUID falls back to its first 8 ASCII letters and digits.
/// Returns `None` when that leaves nothing.
#[must_use]
pub fn library_id(edition_id: &str) -> Option<String> {
    let short = match Uuid::parse_str(edition_id) {
        Ok(uuid) => {
            let bytes = uuid.as_bytes();
            let bits = bytes[..5]
                .iter()
                .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
            (0..8)
                .rev()
                .map(|digit| {
                    // Masked to five bits, so the conversion always succeeds.
                    let index = u8::try_from((bits >> (digit * 5)) & 0x1f).unwrap_or(0);
                    char::from(CROCKFORD[usize::from(index)])
                })
                .collect()
        }
        Err(_) => edition_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect::<String>(),
    };
    Some(short).filter(|short| !short.is_empty())
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
        narrators: Vec::new(),
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

/// The outbox row for one resolver event; `holding_id` is the id a new
/// holding is inserted with.
fn link_event(listing: &resolve::Listing, event: &resolve::Event, holding_id: &str) -> LinkEvent {
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
                format!("holding_added:{holding_id}"),
                serde_json::json!({
                    "listingId": listing_id,
                    "holder": holder.stored(),
                    "holdingId": holding_id,
                }),
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

/// How long a stored listing's `observed_at` may lag before a rescan that
/// changes nothing else still writes it.
const OBSERVED_REFRESH: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// True when a rescan of an otherwise unchanged listing must still write
/// it: the store changed its title or categories, or `observed_at` is
/// older than [`OBSERVED_REFRESH`] (or unreadable).
fn listing_needs_refresh(
    row: &store_listings::Model,
    listing: &resolve::Listing,
    now: chrono::DateTime<Utc>,
) -> bool {
    let stale = chrono::DateTime::parse_from_rfc3339(&row.observed_at).map_or(true, |seen| {
        now.signed_duration_since(seen) > OBSERVED_REFRESH
    });
    row.title != listing.title
        || listing
            .categories
            .as_ref()
            .is_some_and(|c| row.categories.as_ref() != Some(c))
        || stale
}

/// The batch parameters for `plan`.
fn link_params(
    listing: &resolve::Listing,
    plan: &resolve::Plan,
    snapshot: &resolve::Snapshot,
) -> ListingLinkParams {
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
    let holding_id = Uuid::new_v4().to_string();
    ListingLinkParams {
        listing_id: plan.listing.id.clone(),
        listing_is_new: plan.listing_is_new,
        plugin_key: listing.key.plugin_key.clone(),
        marketplace: listing.key.marketplace.clone(),
        product_id: listing.key.product_id.clone(),
        title: listing.title.clone(),
        categories: listing.categories.clone(),
        edition_id: plan.listing.edition_id.clone(),
        stored_edition_id: snapshot
            .listing
            .as_ref()
            .filter(|row| !plan.listing_is_new && row.key == listing.key)
            .map(|row| row.edition_id.clone()),
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
        expression_fill: plan.expression_update.as_ref().map(|x| LinkExpressionFill {
            expression_id: x.id.clone(),
            kind: x.kind.as_str().to_string(),
            language: x.language.clone(),
            abridgement: x.abridgement.as_str().to_string(),
            credit_key: x.credit_key.clone(),
        }),
        work_parent: plan.work_parent.as_ref().map(|link| LinkWorkParent {
            work_id: link.work_id.clone(),
            parent_work_id: link.parent_work_id.clone(),
        }),
        known_rivals: plan
            .new_work
            .as_ref()
            .map(|w| {
                snapshot
                    .works
                    .iter()
                    .filter(|o| {
                        !w.match_key.is_empty()
                            && o.match_key == w.match_key
                            && o.kind == w.kind
                            && match w.kind {
                                resolve::WorkKind::PodcastEpisode => {
                                    o.parent_work_id == w.parent_work_id
                                }
                                resolve::WorkKind::Book
                                | resolve::WorkKind::PodcastShow
                                | resolve::WorkKind::Drama => true,
                            }
                    })
                    .map(|o| o.id.clone())
                    .collect()
            })
            .unwrap_or_default(),
        work_match_key: plan.work_match_key.as_ref().map(|fill| LinkWorkMatchKey {
            work_id: fill.work_id.clone(),
            kind: fill.kind.as_str().to_string(),
            parent_work_id: fill.parent_work_id.clone(),
            match_key: fill.match_key.clone(),
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
        holding: plan.holding.as_ref().map(|h| LinkHoldingRow {
            holder: h.holder.stored(),
            holding_id: holding_id.clone(),
            entitlement: h.entitlement.as_str().to_string(),
            purchased_at: h.purchased_at.clone(),
        }),
        conflicts: plan.conflicts.iter().map(conflict_row).collect(),
        events: plan
            .events
            .iter()
            .map(|e| link_event(listing, e, &holding_id))
            .collect(),
    }
}

/// The credit and series batches for `plan`, each within
/// [`CREDIT_BATCH_STATEMENTS`]; every batch carries the contributors and
/// series its rows reference.
fn credit_batches(plan: &resolve::Plan) -> Vec<CreditBatchParams> {
    let mut batches = Vec::new();
    let mut batch = CreditBatchParams::default();
    // Highest billing positions first, so each target's first-billed credit
    // (the snapshot's "credited" marker) is committed after the rest of it.
    let mut writes: Vec<&resolve::CreditWrite> = plan.credits.iter().collect();
    writes.sort_by_key(|w| std::cmp::Reverse(w.position));
    for write in writes {
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
    // A work's series go in one batch: the snapshot treats any stored entry
    // as the whole set, so they must never be part-written.
    if batch.statement_count() > 0
        && batch.statement_count() + 2 * plan.series.len() > CREDIT_BATCH_STATEMENTS
    {
        batches.push(std::mem::take(&mut batch));
    }
    for write in &plan.series {
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

/// A title someone asks about (a wish, a catalog card): what the resolver
/// needs to say which work it belongs to.
#[derive(Debug, Clone, Default)]
pub struct TitleQuery<'a> {
    /// Display title.
    pub title: &'a str,
    /// Comma-separated authors, when known.
    pub authors: Option<&'a str>,
    /// Audible ASIN, when known.
    pub asin: Option<&'a str>,
    /// ISBN, when known.
    pub isbn: Option<&'a str>,
    /// Series name, when known.
    pub series: Option<&'a str>,
    /// Position in the series, when known.
    pub series_index: Option<&'a str>,
}

/// Plugin key of the synthetic listing a [`TitleQuery`] is resolved as. It
/// names no plugin, so it never matches a stored listing.
const TITLE_QUERY_PLUGIN: &str = "bookclerk-query";

/// The listing the resolver sees for a [`TitleQuery`]: a book, with the
/// query's identifiers, author credits and series. Kind, language and
/// abridgement are left unknown, so the resolver only joins an existing
/// edition through a shared identifier and otherwise finds the work by its
/// identifiers or its title-and-author match key.
///
/// # Returns
///
/// `None` when the query cannot form a valid listing (a blank title, or a
/// field over the listing limits): no stored work can match it.
fn listing_from_query(query: &TitleQuery<'_>) -> Option<resolve::Listing> {
    let mut identifiers = Vec::new();
    if let Some(asin) = query.asin.map(str::trim).filter(|a| !a.is_empty()) {
        identifiers.push((resolve::IdentifierScheme::Asin, asin.to_string(), None));
    }
    if let Some(isbn) = query.isbn.map(str::trim).filter(|i| !i.is_empty()) {
        identifiers.push((resolve::IdentifierScheme::Isbn, isbn.to_string(), None));
    }
    let credits = query
        .authors
        .map(resolve::names::split_display_names)
        .unwrap_or_default()
        .into_iter()
        .map(|name| (name, resolve::CreditRole::Author, None))
        .collect();
    let product_id = query
        .asin
        .or(query.isbn)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("title:{}", query.title.trim().to_lowercase()));
    let draft = resolve::ListingDraft {
        plugin_key: TITLE_QUERY_PLUGIN.to_string(),
        marketplace: None,
        product_id,
        work_kind: Some(resolve::WorkKind::Book),
        expression_kind: None,
        media_kind: None,
        language: None,
        abridgement: None,
        title: query.title.to_string(),
        subtitle: None,
        published_at: None,
        duration_minutes: None,
        categories: None,
        identifiers,
        credits,
        series: query
            .series
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| (s.to_string(), query.series_index.map(str::to_string)))
            .into_iter()
            .collect(),
        parent_product_id: None,
    };
    match resolve::Listing::from_draft(&draft) {
        Ok(listing) => Some(listing),
        Err(err) => {
            tracing::debug!(error = %err, "title query is not a valid listing; it matches no work");
            None
        }
    }
}

/// Namespace of the name-based (version 5) UUIDs that catalog work ids are.
const CATALOG_WORK_NAMESPACE: Uuid = Uuid::from_u128(0x2d7a_91c4_0b5e_5f38_a6e2_7c19_d40b_83f6);

/// The id of a catalog work (a work a wish made, with no edition): a
/// version 5 UUID over its kind and match key, so every wish for the same
/// new book, on any node, names the same work. A work with no match key
/// (no title words) gets the resolver's fresh id instead.
fn catalog_work_id(work: &resolve::WorkRow) -> String {
    if work.match_key.is_empty() {
        return work.id.clone();
    }
    let name = format!("work\0{}\0{}", work.kind.as_str(), work.match_key);
    Uuid::new_v5(&CATALOG_WORK_NAMESPACE, name.as_bytes()).to_string()
}

/// The ids a catalog work's rows are written under, in place of the fresh
/// ids the dry-run plan minted.
struct CatalogIds {
    /// `(planned, written)` work id.
    work: (String, String),
    /// `(planned, written)` expression id, when the plan made one.
    expression: Option<(String, String)>,
    /// `(planned, written)` edition id, when the plan made one.
    edition: Option<(String, String)>,
}

impl CatalogIds {
    /// The ids for `plan`'s new rows, with the work written as `work_id`.
    ///
    /// The expression's id is a version 5 UUID over the work id and the
    /// expression's kind, language, abridgement and credit key; the
    /// edition keeps the plan's id, which is already derived from the
    /// query's listing key ([`edition_id_for_listing`]).
    fn new(plan: &resolve::Plan, work_id: &str) -> Self {
        let planned_work = plan
            .new_work
            .as_ref()
            .map(|w| w.id.clone())
            .unwrap_or_default();
        let expression = plan.new_expression.as_ref().map(|x| {
            let name = format!(
                "expression\0{work_id}\0{}\0{}\0{}\0{}",
                x.kind.as_str(),
                x.language.as_deref().unwrap_or(""),
                x.abridgement.as_str(),
                x.credit_key
            );
            (
                x.id.clone(),
                Uuid::new_v5(&CATALOG_WORK_NAMESPACE, name.as_bytes()).to_string(),
            )
        });
        let edition = plan
            .new_edition
            .as_ref()
            .map(|e| (e.id.clone(), e.id.clone()));
        Self {
            work: (planned_work, work_id.to_string()),
            expression,
            edition,
        }
    }

    /// The written id of the planned row `planned` at `level`, or `None`
    /// when it is not one of the catalog rows.
    fn remap(&self, level: resolve::Level, planned: &str) -> Option<String> {
        let pair = match level {
            resolve::Level::Work => Some(&self.work),
            resolve::Level::Expression => self.expression.as_ref(),
            resolve::Level::Edition => self.edition.as_ref(),
        }?;
        (pair.0 == planned).then(|| pair.1.clone())
    }
}

/// The statements that write a catalog work's rows: the work, then its
/// expression and edition when the plan made them, then the edition's
/// identifiers, each `INSERT OR IGNORE` (canonical SQL-v1) so a repeat or a
/// concurrent wish is a no-op. A strong identifier another edition already
/// carries is skipped by its unique key.
///
/// # Arguments
///
/// * `plan` - The dry-run plan for the wish's query.
/// * `work` - The plan's new work.
/// * `ids` - The ids the rows are written under.
/// * `now` - `created_at` / `updated_at` (RFC 3339).
fn catalog_statements(
    plan: &resolve::Plan,
    work: &resolve::WorkRow,
    ids: &CatalogIds,
    now: &str,
) -> Vec<bookclerk_plugin_abi::TypedDbStatement> {
    use crate::control_plane::batch::{exec, int_opt, text, text_opt};
    let mut statements = vec![exec(
        "INSERT OR IGNORE INTO works (id, kind, title, match_key, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            text(&ids.work.1),
            text(work.kind.as_str()),
            text(&work.title),
            text(&work.match_key),
            text(now),
            text(now),
        ],
    )];
    let (Some(x), Some((_, expression_id))) = (&plan.new_expression, &ids.expression) else {
        return statements;
    };
    statements.push(exec(
        "INSERT OR IGNORE INTO expressions \
         (id, work_id, kind, language, abridgement, credit_key, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            text(expression_id),
            text(&ids.work.1),
            text(x.kind.as_str()),
            text_opt(x.language.as_deref()),
            text(x.abridgement.as_str()),
            text(&x.credit_key),
            text(now),
            text(now),
        ],
    ));
    let (Some(e), Some((_, edition_id))) = (&plan.new_edition, &ids.edition) else {
        return statements;
    };
    statements.push(exec(
        "INSERT OR IGNORE INTO editions \
         (id, expression_id, media_kind, title, subtitle, published_at, duration_minutes, \
          created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            text(edition_id),
            text(expression_id),
            text(e.media_kind.as_str()),
            text(&e.title),
            text_opt(e.subtitle.as_deref()),
            text_opt(e.published_at.as_deref()),
            int_opt(e.duration_minutes),
            text(now),
            text(now),
        ],
    ));
    for row in plan
        .edition_identifiers
        .iter()
        .filter(|row| &row.edition_id == edition_id)
    {
        statements.push(exec(
            "INSERT OR IGNORE INTO edition_identifiers \
             (edition_id, scheme, value, marketplace, strength, strong_key, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(edition_id),
                text(row.key.scheme.as_str()),
                text(&row.key.value),
                text(&row.key.marketplace),
                text(row.key.strength().as_str()),
                text_opt(row.key.strong_key().as_deref()),
                text(now),
            ],
        ));
    }
    statements
}

/// The work a dry-run plan reached, or `None` when it would make a new one.
fn planned_work_id(plan: &resolve::Plan, snapshot: &resolve::Snapshot) -> Option<String> {
    if plan.new_work.is_some() {
        return None;
    }
    if let Some(expression) = &plan.new_expression {
        return Some(expression.work_id.clone());
    }
    let expression_id = match &plan.new_edition {
        Some(edition) => edition.expression_id.clone(),
        None => snapshot
            .editions
            .iter()
            .find(|e| e.id == plan.listing.edition_id)?
            .expression_id
            .clone(),
    };
    snapshot
        .expressions
        .iter()
        .find(|e| e.id == expression_id)
        .map(|e| e.work_id.clone())
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
    /// Ids bound per `IN (...)` lookup on this store's backend.
    pub(super) fn lookup_chunk(&self) -> usize {
        lookup_chunk(self.max_binds)
    }

    /// True when an edition with id `edition_id` is in the library.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    async fn edition_exists(&self, edition_id: &str) -> Result<bool> {
        use sea_orm::PaginatorTrait as _;
        let found = editions::Entity::find_by_id(edition_id.to_string())
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(found > 0)
    }

    /// The work in the library that `query` belongs to, when the library
    /// holds one.
    ///
    /// The query is resolved exactly as a scanned listing would be (shared
    /// identifiers first, then the work's identifiers, then the work-level
    /// title-and-author match), but nothing is written. A query that would
    /// make a new work, or whose work no library title resolves to, is not
    /// owned: *Dune Messiah* is its own work beside *Dune*. A query that
    /// cannot form a valid listing (a blank title, or a field over the
    /// listing limits) matches no work, so it is not owned either.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read fails.
    pub async fn owned_work_for(&self, query: &TitleQuery<'_>) -> Result<Option<String>> {
        let Some(work_id) = self.work_for(query).await? else {
            return Ok(None);
        };
        if self.book_uuids_for_work(&work_id).await?.is_empty() {
            return Ok(None);
        }
        Ok(Some(work_id))
    }

    /// The existing work `query` belongs to, held by a library title or
    /// not, when the resolver finds one; `None` when it would make a new
    /// work. Nothing is written. A wish stores this id when it is created,
    /// so the queue and the shelves compare works, not words.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read fails. A query that cannot form
    /// a valid listing (a blank title, or a field over the listing limits)
    /// names no work.
    pub async fn work_for(&self, query: &TitleQuery<'_>) -> Result<Option<String>> {
        let Some(listing) = listing_from_query(query) else {
            return Ok(None);
        };
        let (plan, snapshot) = self.dry_run(&listing).await?;
        Ok(planned_work_id(&plan, &snapshot))
    }

    /// Resolves `listing` against the stored rows without writing: the
    /// plan [`Self::record_listing`] would write, and the snapshot it read.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read fails.
    async fn dry_run(
        &self,
        listing: &resolve::Listing,
    ) -> Result<(resolve::Plan, resolve::Snapshot)> {
        let holder = resolve::Holder::Account(String::from("query"));
        let holding = resolve::HoldingDraft {
            holder: holder.clone(),
            entitlement: resolve::Entitlement::Purchase,
            purchased_at: None,
        };
        let snapshot = self.listing_snapshot(listing, &holder).await?;
        let plan = resolve::resolve(listing, &holding, &snapshot, &mut UuidIds::default());
        Ok((plan, snapshot))
    }

    /// The work a wish for `query` is for, made when the library has none.
    ///
    /// The query is resolved exactly as [`Self::work_for`] resolves it. When
    /// that names an existing work (held by a library title or not), that is
    /// the work. Otherwise a catalog work is written as one atomic batch:
    /// the work, an expression and an edition carrying the wish's ASIN and
    /// ISBN (a non-acquirable catalog edition, as the data model ADR keeps
    /// catalog results), then their credits and series. No store listing or
    /// holding is written, so the library does not hold it. A later wish
    /// with the same identifiers joins that edition, and a later scan of the
    /// book finds the work through the work matcher (same title, authors
    /// and kind), so wishes and the scanned title share the work.
    ///
    /// Every id is derived (the work's from its match key, the edition's
    /// from the query, the expression's from both) and every row is
    /// inserted with `INSERT OR IGNORE`, so two wishes for one new book made
    /// at once (on one node or two) name the same rows.
    ///
    /// # Arguments
    ///
    /// * `query` - The wished title, authors, identifiers and series.
    ///
    /// # Returns
    ///
    /// The work, and the existing edition the query's identifiers reached
    /// (`None` when they reached none).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when the query cannot form a valid
    /// listing (a blank title, or a field over the listing limits), and
    /// [`LibraryError`] when a read or write fails.
    pub async fn work_for_wish(&self, query: &TitleQuery<'_>) -> Result<(String, Option<String>)> {
        let Some(listing) = listing_from_query(query) else {
            return Err(LibraryError::Invalid(String::from(
                "a wish needs a title within the listing limits",
            )));
        };
        let (plan, snapshot) = self.dry_run(&listing).await?;
        let edition = plan
            .new_edition
            .is_none()
            .then(|| plan.listing.edition_id.clone());
        if let Some(work_id) = planned_work_id(&plan, &snapshot) {
            return Ok((work_id, edition));
        }
        let Some(planned) = &plan.new_work else {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "the resolver reached no work for wish {:?}",
                listing.title
            )));
        };
        let work_id = catalog_work_id(planned);
        let ids = CatalogIds::new(&plan, &work_id);
        let statements = catalog_statements(&plan, planned, &ids, &now_str());
        let operation_id = format!("catalog-work-{}", Uuid::new_v4());
        self.execute_host_batch(crate::control_plane::batch::request(
            &operation_id,
            statements,
        ))
        .await?;
        let mut catalog_plan = plan.clone();
        catalog_plan.credits = plan
            .credits
            .iter()
            .filter_map(|c| {
                let mut c = c.clone();
                c.target_id = ids.remap(c.level, &c.target_id)?;
                Some(c)
            })
            .collect();
        catalog_plan.series = plan
            .series
            .iter()
            .filter(|s| s.work_id == planned.id)
            .cloned()
            .map(|mut s| {
                s.work_id.clone_from(&work_id);
                s
            })
            .collect();
        for batch in credit_batches(&catalog_plan) {
            self.run_write_credits(batch).await?;
        }
        Ok((work_id, ids.edition.map(|(_, id)| id)))
    }

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
        self.link_listing(listing, Some(holding)).await
    }

    /// Resolves a catalog `listing` (a title a store sells that nobody in
    /// the library holds) into the library: its work, expression, edition
    /// and `store_listings` row, with no holding.
    ///
    /// A later scan of the same product finds the same listing row and adds
    /// only its holding, so a catalog listing and a held one never diverge.
    ///
    /// # Returns
    ///
    /// The plan that was written (a no-op plan when nothing changed).
    ///
    /// # Errors
    ///
    /// As [`Self::record_listing`].
    pub async fn record_catalog_listing(
        &self,
        listing: &resolve::Listing,
    ) -> Result<resolve::Plan> {
        self.link_listing(listing, None).await
    }

    /// [`Self::record_listing`] and [`Self::record_catalog_listing`]: links
    /// `listing`, recording `holding` when one is given.
    ///
    /// # Errors
    ///
    /// As [`Self::try_link_listing`], plus [`LibraryError::Other`] when
    /// other writers kept changing the rows for every attempt.
    async fn link_listing(
        &self,
        listing: &resolve::Listing,
        holding: Option<&resolve::HoldingDraft>,
    ) -> Result<resolve::Plan> {
        self.try_link_listing(listing, holding)
            .await?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "listing {} kept changing under concurrent writers",
                    listing.key.product_id
                ))
            })
    }

    /// Links `listing` like [`Self::link_listing`], but reports running out
    /// of attempts apart from failures.
    ///
    /// # Returns
    ///
    /// The plan that was written, or `None` when other writers changed the
    /// rows between the read and the write on every one of the
    /// `LINK_ATTEMPTS` attempts (nothing was written).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read or write fails or when the
    /// stored rows are inconsistent.
    pub(super) async fn try_link_listing(
        &self,
        listing: &resolve::Listing,
        holding: Option<&resolve::HoldingDraft>,
    ) -> Result<Option<resolve::Plan>> {
        // A catalog listing has no holder; a holder no row names finds no
        // stored holding, which a catalog plan ignores anyway.
        let holder = holding.map_or_else(
            || resolve::Holder::Account(String::new()),
            |h| h.holder.clone(),
        );
        let plan_for = |snapshot: &resolve::Snapshot, ids: &mut UuidIds| match holding {
            Some(holding) => resolve::resolve(listing, holding, snapshot, ids),
            None => resolve::resolve_catalog(listing, snapshot, ids),
        };
        for _ in 0..LINK_ATTEMPTS {
            let (snapshot, stored_row) = self.listing_snapshot_and_row(listing, &holder).await?;
            if let Some(missing) = snapshot.is_closed() {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "library rows for listing {} reference a missing {missing}",
                    listing.key.product_id
                )));
            }
            let mut plan = plan_for(&snapshot, &mut UuidIds::default());
            if let Some(edition) = &plan.new_edition {
                if self.edition_exists(&edition.id).await? {
                    plan = plan_for(
                        &snapshot,
                        &mut UuidIds {
                            random_editions: true,
                        },
                    );
                }
            }
            let refresh = stored_row
                .as_ref()
                .is_some_and(|row| listing_needs_refresh(row, listing, Utc::now()));
            if !plan.is_noop() || !plan.conflicts.is_empty() || refresh {
                let params = link_params(listing, &plan, &snapshot);
                if self.run_link_listing(params).await? != atomic_status::OK {
                    continue;
                }
            }
            for batch in credit_batches(&plan) {
                self.run_write_credits(batch).await?;
            }
            return Ok(Some(plan));
        }
        Ok(None)
    }

    /// Resolves a page of listings, each with its holding and the fields
    /// its library title offers its work, logging a listing that fails
    /// rather than returning it (see [`LibraryStore::record_listing`]).
    ///
    /// The rows every listing's snapshot reads are read once for the page
    /// ([`ListingPool::load`]). A listing the library already holds as the
    /// store describes it resolves to a plan with nothing to write, and is
    /// settled from those rows without another read; every other listing
    /// goes through [`Self::record_listing`], which reads its own snapshot
    /// and writes under the usual guards. Settling the unchanged listings
    /// first is the same as scanning them first, which a scan is free to do.
    pub(crate) async fn record_listing_page(
        &self,
        items: &[(resolve::Listing, resolve::HoldingDraft, WorkEnrichment)],
    ) {
        let Some((_, first, _)) = items.first() else {
            return;
        };
        let listings: Vec<&resolve::Listing> = items.iter().map(|(l, _, _)| l).collect();
        let pool = match ListingPool::load(self, &listings, &first.holder).await {
            Ok(pool) => Some(pool),
            Err(err) => {
                tracing::warn!(error = %err, "page of listings not read ahead; resolving each on its own");
                None
            }
        };
        // The pool is read before the page writes anything, so a listing
        // whose key, parent or edition an earlier entry of this page wrote
        // is resolved against the store, never against the stale pool: an
        // episode whose show was first stored on this page gains its link.
        let mut written_keys: std::collections::HashSet<resolve::ListingKey> =
            std::collections::HashSet::new();
        let mut written_editions: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (listing, holding, fill) in items {
            let settled = match &pool {
                Some(_)
                    if written_keys.contains(&listing.key)
                        || listing
                            .parent_key()
                            .is_some_and(|parent| written_keys.contains(&parent)) =>
                {
                    Ok(None)
                }
                Some(pool) => self
                    .settled_listing(pool, listing, holding)
                    .await
                    .map(|plan| {
                        plan.filter(|plan| !written_editions.contains(&plan.listing.edition_id))
                    }),
                None => Ok(None),
            };
            let result = match settled {
                Ok(Some(plan)) => {
                    let edition_id = &plan.listing.edition_id;
                    let known = pool.as_ref().and_then(|p| p.work_of_edition(edition_id));
                    if known.is_some_and(|work| !fill.fills(work)) {
                        Ok(())
                    } else {
                        self.fill_work_from_book(edition_id, fill).await
                    }
                }
                Ok(None) | Err(_) => {
                    written_keys.insert(listing.key.clone());
                    match self.record_listing(listing, holding).await {
                        Ok(plan) => {
                            written_editions.insert(plan.listing.edition_id.clone());
                            self.fill_work_from_book(&plan.listing.edition_id, fill)
                                .await
                        }
                        Err(err) => Err(err),
                    }
                }
            };
            if let Err(err) = result {
                tracing::warn!(
                    product_id = %listing.key.product_id,
                    plugin_key = %listing.key.plugin_key,
                    error = %err,
                    "library title stored but not resolved into the library model; the next scan retries"
                );
            }
        }
    }

    /// The plan `listing` resolves to against `pool`, when that plan writes
    /// nothing: no new or changed rows, no conflicts, no credits or series,
    /// and no stale store metadata to refresh.
    ///
    /// # Returns
    ///
    /// `None` when the listing has something to write, or its snapshot
    /// from the pool is not closed; the caller resolves it on its own.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read the pool does not cover fails.
    async fn settled_listing(
        &self,
        pool: &ListingPool,
        listing: &resolve::Listing,
        holding: &resolve::HoldingDraft,
    ) -> Result<Option<resolve::Plan>> {
        let reads = SnapshotReader {
            store: self,
            pool: Some(pool),
        };
        let (snapshot, stored_row) = self
            .listing_snapshot_from(reads, listing, &holding.holder)
            .await?;
        if snapshot.is_closed().is_some() {
            return Ok(None);
        }
        let plan = resolve::resolve(listing, holding, &snapshot, &mut UuidIds::default());
        let refresh = stored_row
            .as_ref()
            .is_some_and(|row| listing_needs_refresh(row, listing, Utc::now()));
        let settled = plan.is_noop()
            && plan.conflicts.is_empty()
            && !refresh
            && credit_batches(&plan).is_empty();
        Ok(settled.then_some(plan))
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
        Ok(self.listing_snapshot_and_row(listing, holder).await?.0)
    }

    /// [`Self::listing_snapshot`], plus the stored listing row itself, whose
    /// store metadata (title, categories, `observed_at`) the resolver does
    /// not see.
    async fn listing_snapshot_and_row(
        &self,
        listing: &resolve::Listing,
        holder: &resolve::Holder,
    ) -> Result<(resolve::Snapshot, Option<store_listings::Model>)> {
        self.listing_snapshot_from(
            SnapshotReader {
                store: self,
                pool: None,
            },
            listing,
            holder,
        )
        .await
    }

    /// [`Self::listing_snapshot_and_row`] through `reads`, which answers
    /// from a page's [`ListingPool`] where it covers the ask.
    pub(super) async fn listing_snapshot_from(
        &self,
        reads: SnapshotReader<'_>,
        listing: &resolve::Listing,
        holder: &resolve::Holder,
    ) -> Result<(resolve::Snapshot, Option<store_listings::Model>)> {
        let stored_row = reads.listing(&listing.key).await?;
        let stored = stored_row.clone().map(listing_row);
        let parent = match listing.parent_key() {
            Some(key) => reads.listing(&key).await?.map(listing_row),
            None => None,
        };

        let mut overrides = Vec::new();
        if let Some(row) = &stored {
            let pins = reads.pins(std::slice::from_ref(&row.id)).await?;
            overrides.extend(pins.into_iter().filter_map(|m| {
                m.target_id.map(|edition_id| resolve::Override::PinListing {
                    listing: listing.key.clone(),
                    edition_id,
                })
            }));
        }

        // Identifier rows sharing one of the listing's edition identifiers.
        let keys: Vec<_> = listing
            .edition_identifiers()
            .map(edition_identifier_key)
            .collect();
        let mut identifier_rows = BTreeSet::new();
        for m in reads.edition_identifiers_matching(&keys).await? {
            identifier_rows.insert(edition_identifier_row(m)?);
        }

        let mut edition_ids: BTreeSet<String> = BTreeSet::new();
        edition_ids.extend(stored.iter().map(|l| l.edition_id.clone()));
        edition_ids.extend(parent.iter().map(|l| l.edition_id.clone()));
        edition_ids.extend(overrides.iter().filter_map(|o| match o {
            resolve::Override::PinListing { edition_id, .. } => Some(edition_id.clone()),
            resolve::Override::IsolateWork { .. } => None,
        }));
        edition_ids.extend(identifier_rows.iter().map(|r| r.edition_id.clone()));

        let edition_list: Vec<String> = edition_ids.iter().cloned().collect();
        let mut editions_found = Vec::new();
        for m in reads.editions(&edition_list).await? {
            editions_found.push(edition_row(m)?);
        }
        for m in reads.edition_identifiers_of(&edition_list).await? {
            identifier_rows.insert(edition_identifier_row(m)?);
        }

        let mut work_identifier_rows = Vec::new();
        let work_keys: Vec<_> = listing
            .work_identifiers()
            .map(work_identifier_key)
            .collect();
        for m in reads.work_identifiers_matching(&work_keys).await? {
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

        let expression_ids: Vec<String> = editions_found
            .iter()
            .map(|e| e.expression_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut expressions_found = Vec::new();
        for m in reads.expressions(&expression_ids).await? {
            expressions_found.push(expression_row(m)?);
        }

        let mut work_ids: BTreeSet<String> = expressions_found
            .iter()
            .map(|x| x.work_id.clone())
            .collect();
        work_ids.extend(work_identifier_rows.iter().map(|r| r.work_id.clone()));
        let match_key = listing.work_match_key();
        if !match_key.is_empty() {
            work_ids.extend(
                reads
                    .works_matching(&match_key, listing.work_kind.as_str())
                    .await?,
            );
        }

        let work_list: Vec<String> = work_ids.iter().cloned().collect();
        let mut works_found = Vec::new();
        for m in reads.works(&work_list).await? {
            works_found.push(work_row(&m)?);
        }
        for m in reads.expressions_of_works(&work_list).await? {
            if !expressions_found.iter().any(|x| x.id == m.id) {
                expressions_found.push(expression_row(m)?);
            }
        }
        overrides.extend(reads.isolations(&work_list).await?.into_iter().map(|m| {
            resolve::Override::IsolateWork {
                work_id: m.subject_id,
            }
        }));

        let holding = match &stored {
            Some(row) => reads
                .holdings(std::slice::from_ref(&row.id), &holder.stored())
                .await?
                .into_iter()
                .next()
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

        let all_expression_ids: Vec<String> = expressions_found
            .iter()
            .map(|x| x.id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut credited = BTreeSet::new();
        for (level, ids) in [
            (resolve::Level::Work, &work_list),
            (resolve::Level::Expression, &all_expression_ids),
            (resolve::Level::Edition, &edition_list),
        ] {
            // A target counts as credited once its first-billed credit is
            // stored: `credit_batches` writes it last, so a credit write
            // that stopped part-way leaves the target uncredited and the
            // next scan finishes it.
            credited.extend(
                reads
                    .first_credits(level, ids)
                    .await?
                    .into_iter()
                    .map(|id| (level, id)),
            );
        }
        let works_with_series: Vec<String> = reads
            .works_with_series(&work_list)
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        // Both readers can return rows in any order; sort the lists so a
        // snapshot does not depend on which one answered.
        editions_found.sort_by(|a, b| a.id.cmp(&b.id));
        expressions_found.sort_by(|a, b| a.id.cmp(&b.id));
        works_found.sort_by(|a, b| a.id.cmp(&b.id));
        work_identifier_rows.sort_by(|a, b| {
            (a.key.scheme.as_str(), &a.key.value).cmp(&(b.key.scheme.as_str(), &b.key.value))
        });

        let snapshot = resolve::Snapshot {
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
        };
        Ok((snapshot, stored_row))
    }

    /// Loads many works by id, with their work-level credits, narrators and
    /// series, in chunked bulk reads.
    ///
    /// # Arguments
    ///
    /// * `ids` - Work ids; unknown ids are left out of the result.
    ///
    /// # Returns
    ///
    /// The works found, keyed by id.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn works_by_ids(
        &self,
        ids: &[String],
    ) -> Result<std::collections::BTreeMap<String, WorkRecord>> {
        let mut found: Vec<WorkRecord> = find_chunked(self, ids, |part| {
            works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(map_work)
        .collect();
        self.fill_work_names(&mut found).await?;
        Ok(found.into_iter().map(|w| (w.id.clone(), w)).collect())
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

    /// Lists up to `limit` works with ids after `after_id`, in id order, with
    /// their work-level credits and series.
    ///
    /// A keyset page: pass the last id of one page as `after_id` of the
    /// next, and stop at a short page. Unlike [`Self::list_works`] it never
    /// reads the whole table at once.
    ///
    /// # Arguments
    ///
    /// * `after_id` - Exclusive lower bound on `works.id`; `None` starts at
    ///   the first work.
    /// * `limit` - Page size, clamped to 1..=500.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn list_works_page(
        &self,
        after_id: Option<&str>,
        limit: u64,
    ) -> Result<Vec<WorkRecord>> {
        let mut query = works::Entity::find().order_by_asc(works::Column::Id);
        if let Some(after) = after_id {
            query = query.filter(works::Column::Id.gt(after));
        }
        let mut works: Vec<WorkRecord> = query
            .limit(limit.clamp(1, 500))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_work)
            .collect();
        self.fill_work_names(&mut works).await?;
        Ok(works)
    }

    /// Stored text hashes of the `model` embeddings of `target_kind` targets
    /// among `target_ids`, keyed by target id. Targets with no embedding are
    /// absent.
    ///
    /// Reads in chunks sized from the adapter's bind limit, so one call
    /// replaces a lookup per target.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn embedding_text_hashes(
        &self,
        target_kind: &str,
        model: &str,
        target_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        /// One stored hash, decoded by column name: the host database proxy
        /// does not keep the select order, so a positional tuple read could
        /// swap the columns.
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct StoredHash {
            /// `embeddings.target_id`.
            target_id: String,
            /// `embeddings.text_hash`.
            text_hash: String,
        }
        let mut hashes = std::collections::HashMap::with_capacity(target_ids.len());
        for chunk in target_ids.chunks(self.lookup_chunk()) {
            let rows = embeddings::Entity::find()
                .select_only()
                .column(embeddings::Column::TargetId)
                .column(embeddings::Column::TextHash)
                .filter(embeddings::Column::TargetKind.eq(target_kind))
                .filter(embeddings::Column::Model.eq(model))
                .filter(embeddings::Column::TargetId.is_in(chunk.iter().cloned()))
                .into_model::<StoredHash>()
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            hashes.extend(rows.into_iter().map(|row| (row.target_id, row.text_hash)));
        }
        Ok(hashes)
    }

    /// Deletes work embeddings (every model) whose work no longer exists,
    /// and answers how many it removed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn prune_orphan_work_embeddings(&self) -> Result<u64> {
        use sea_orm::sea_query::Query;
        Ok(embeddings::Entity::delete_many()
            .filter(embeddings::Column::TargetKind.eq("work"))
            .filter(
                embeddings::Column::TargetId.not_in_subquery(
                    Query::select()
                        .column(works::Column::Id)
                        .from(works::Entity)
                        .to_owned(),
                ),
            )
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .rows_affected)
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

    /// Fills each work's credited names, its expressions' narrators and
    /// its series names.
    async fn fill_work_names(&self, works: &mut [WorkRecord]) -> Result<()> {
        use crate::entities::{contributors, series};
        use std::collections::BTreeMap;
        if works.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = works.iter().map(|w| w.id.clone()).collect();
        let mut work_credits = Vec::new();
        let mut entries = Vec::new();
        for chunk in ids.chunks(self.lookup_chunk()) {
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
        // Narrators are credited on expressions, not on the work.
        let mut expression_work: BTreeMap<String, String> = BTreeMap::new();
        for chunk in ids.chunks(self.lookup_chunk()) {
            expression_work.extend(
                expressions::Entity::find()
                    .filter(expressions::Column::WorkId.is_in(chunk.iter().cloned()))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .into_iter()
                    .map(|x| (x.id, x.work_id)),
            );
        }
        let expression_ids: Vec<String> = expression_work.keys().cloned().collect();
        let mut narrator_credits = Vec::new();
        for chunk in expression_ids.chunks(self.lookup_chunk()) {
            narrator_credits.extend(
                credits::Entity::find()
                    .filter(credits::Column::TargetKind.eq(resolve::Level::Expression.as_str()))
                    .filter(credits::Column::TargetId.is_in(chunk.iter().cloned()))
                    .filter(credits::Column::Role.eq(resolve::CreditRole::Narrator.as_str()))
                    .order_by_asc(credits::Column::Position)
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?,
            );
        }
        let contributor_ids: BTreeSet<String> = work_credits
            .iter()
            .chain(&narrator_credits)
            .map(|c| c.contributor_id.clone())
            .collect();
        let series_ids: BTreeSet<String> = entries.iter().map(|e| e.series_id.clone()).collect();
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        let contributor_ids: Vec<String> = contributor_ids.into_iter().collect();
        for chunk in contributor_ids.chunks(self.lookup_chunk()) {
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
        for chunk in series_ids.chunks(self.lookup_chunk()) {
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
            let mut narrators: Vec<String> = Vec::new();
            for credit in narrator_credits
                .iter()
                .filter(|c| expression_work.get(&c.target_id) == Some(&work.id))
            {
                if let Some(name) = names.get(&credit.contributor_id) {
                    if !narrators.contains(name) {
                        narrators.push(name.clone());
                    }
                }
            }
            work.narrators = narrators;
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
        Ok(self
            .work_ids_for_books(&[book_uuid.to_string()])
            .await?
            .remove(book_uuid))
    }

    /// The works many library titles resolve to, read in a few bulk
    /// queries rather than a chain of lookups per title.
    ///
    /// # Arguments
    ///
    /// * `book_uuids` - Library `books.uuid` values.
    ///
    /// # Returns
    ///
    /// Each resolved title's work id, keyed by title UUID; titles not yet
    /// resolved are absent.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn work_ids_for_books(
        &self,
        book_uuids: &[String],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        use crate::entities::books;
        use std::collections::BTreeMap;

        let mut books_found = Vec::new();
        for chunk in book_uuids.chunks(self.lookup_chunk()) {
            books_found.extend(
                books::Entity::find()
                    .filter(books::Column::Uuid.is_in(chunk.iter().cloned()))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?,
            );
        }
        let product_ids: Vec<String> = books_found
            .iter()
            .map(|b| b.product_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut edition_of: BTreeMap<(String, String, String), String> = BTreeMap::new();
        for chunk in product_ids.chunks(self.lookup_chunk()) {
            for l in store_listings::Entity::find()
                .filter(store_listings::Column::ProductId.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                edition_of.insert((l.plugin_key, l.marketplace, l.product_id), l.edition_id);
            }
        }
        let work_of_edition = self
            .works_of_editions(edition_of.values().cloned().collect())
            .await?;
        Ok(books_found
            .into_iter()
            .filter_map(|b| {
                let key = (
                    b.plugin_key,
                    resolve::stored_marketplace(&b.marketplace),
                    b.product_id,
                );
                let work = work_of_edition.get(edition_of.get(&key)?)?;
                Some((b.uuid, work.clone()))
            })
            .collect())
    }

    /// Each edition's work, through its expression.
    pub(super) async fn works_of_editions(
        &self,
        edition_ids: BTreeSet<String>,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        use std::collections::BTreeMap;
        let edition_ids: Vec<String> = edition_ids.into_iter().collect();
        let mut expression_of = BTreeMap::new();
        for chunk in edition_ids.chunks(self.lookup_chunk()) {
            for e in editions::Entity::find()
                .filter(editions::Column::Id.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                expression_of.insert(e.id, e.expression_id);
            }
        }
        let expression_ids: Vec<String> = expression_of
            .values()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut work_of_expression = BTreeMap::new();
        for chunk in expression_ids.chunks(self.lookup_chunk()) {
            for x in expressions::Entity::find()
                .filter(expressions::Column::Id.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                work_of_expression.insert(x.id, x.work_id);
            }
        }
        Ok(expression_of
            .into_iter()
            .filter_map(|(edition, expression)| {
                Some((edition, work_of_expression.get(&expression)?.clone()))
            })
            .collect())
    }

    /// Library title UUIDs whose listings resolve to `work_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn book_uuids_for_work(&self, work_id: &str) -> Result<Vec<String>> {
        Ok(self
            .book_uuids_for_works(&[work_id.to_string()])
            .await?
            .remove(work_id)
            .unwrap_or_default())
    }

    /// The library titles (`books.uuid`) whose listings reach each of
    /// `work_ids`, in chunked bulk reads.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Works to look up.
    ///
    /// # Returns
    ///
    /// Each work with at least one library title, mapped to its titles'
    /// uuids in ascending order; works no title reaches are absent.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn book_uuids_for_works(
        &self,
        work_ids: &[String],
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
        use crate::entities::books;
        use std::collections::BTreeMap;
        let work_of_expression: BTreeMap<String, String> = find_chunked(self, work_ids, |part| {
            expressions::Entity::find()
                .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(|x| (x.id, x.work_id))
        .collect();
        if work_of_expression.is_empty() {
            return Ok(BTreeMap::new());
        }
        let expression_ids: Vec<String> = work_of_expression.keys().cloned().collect();
        let work_of_edition: BTreeMap<String, String> =
            find_chunked(self, &expression_ids, |part| {
                editions::Entity::find()
                    .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .filter_map(|e| Some((e.id, work_of_expression.get(&e.expression_id)?.clone())))
            .collect();
        if work_of_edition.is_empty() {
            return Ok(BTreeMap::new());
        }
        let edition_ids: Vec<String> = work_of_edition.keys().cloned().collect();
        let listings = find_chunked(self, &edition_ids, |part| {
            store_listings::Entity::find()
                .filter(store_listings::Column::EditionId.is_in(part.iter().cloned()))
        })
        .await?;
        let work_of_key: BTreeMap<(String, String, String), String> = listings
            .into_iter()
            .filter_map(|l| {
                let work = work_of_edition.get(&l.edition_id)?.clone();
                Some(((l.plugin_key, l.marketplace, l.product_id), work))
            })
            .collect();
        let product_ids: Vec<String> = work_of_key
            .keys()
            .map(|(_, _, product)| product.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        {
            for b in find_chunked(self, &product_ids, |part| {
                books::Entity::find().filter(books::Column::ProductId.is_in(part.iter().cloned()))
            })
            .await?
            {
                let key = (
                    b.plugin_key,
                    resolve::stored_marketplace(&b.marketplace),
                    b.product_id,
                );
                if let Some(work) = work_of_key.get(&key) {
                    found.entry(work.clone()).or_default().insert(b.uuid);
                }
            }
        }
        Ok(found
            .into_iter()
            .map(|(work, uuids)| (work, uuids.into_iter().collect()))
            .collect())
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
        am.categories = Set(enrichment.categories.clone().or(existing.categories));
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Fills the work an edition belongs to from a library title's
    /// metadata, only where the work has none yet; a work an enricher
    /// already described keeps its own values.
    ///
    /// # Arguments
    ///
    /// * `edition_id` - An edition of the work.
    /// * `fill` - The title's description, subjects, categories and cover.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read or write fails.
    pub(crate) async fn fill_work_from_book(
        &self,
        edition_id: &str,
        fill: &WorkEnrichment,
    ) -> Result<()> {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        if fill.is_empty() {
            return Ok(());
        }
        let Some(work_id) = self
            .works_of_editions(BTreeSet::from([edition_id.to_string()]))
            .await?
            .remove(edition_id)
        else {
            return Ok(());
        };
        let Some(work) = works::Entity::find_by_id(work_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(());
        };
        if !fill.fills(&work) {
            return Ok(());
        }
        let mut am: works::ActiveModel = work.clone().into();
        am.description = Set(work.description.or_else(|| fill.description.clone()));
        am.subjects = Set(work.subjects.or_else(|| fill.subjects.clone()));
        am.categories = Set(work.categories.or_else(|| fill.categories.clone()));
        am.cover_url = Set(work.cover_url.or_else(|| fill.cover_url.clone()));
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// The edition a library title's listing reaches, if it is resolved.
    pub(crate) async fn edition_id_for_book(
        &self,
        book: &crate::entities::books::Model,
    ) -> Result<Option<String>> {
        Ok(store_listings::Entity::find()
            .filter(store_listings::Column::PluginKey.eq(book.plugin_key.as_str()))
            .filter(
                store_listings::Column::Marketplace
                    .eq(resolve::stored_marketplace(&book.marketplace).as_str()),
            )
            .filter(store_listings::Column::ProductId.eq(book.product_id.as_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|l| l.edition_id))
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
        use sea_orm::{sea_query::OnConflict, ActiveValue::Set};
        if scheme.level() != resolve::Level::Work {
            return Ok(());
        }
        let Some(key) = resolve::IdentifierKey::new(scheme, raw, None, "") else {
            return Ok(());
        };
        // The first work to claim an identifier keeps it, including one a
        // concurrent link batch claimed a moment ago.
        let am = work_identifiers::ActiveModel {
            scheme: Set(key.scheme.as_str().to_string()),
            value: Set(key.value),
            work_id: Set(work_id.to_string()),
            created_at: Set(now_str()),
        };
        work_identifiers::Entity::insert(am)
            .on_conflict(
                OnConflict::columns([
                    work_identifiers::Column::Scheme,
                    work_identifiers::Column::Value,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
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
    /// Category labels (serialized).
    pub categories: Option<String>,
    /// HTTPS cover URL.
    pub cover_url: Option<String>,
}

impl WorkEnrichment {
    /// The fields a library title offers its work.
    #[must_use]
    pub fn of_book(book: &crate::models::BookRecord) -> Self {
        Self {
            description: book.description.clone(),
            subjects: book.subjects.clone(),
            categories: book.categories.clone(),
            cover_url: book.cover_url.clone(),
        }
    }

    /// True when writing these fields would change `work`: one of them is
    /// set here and missing there.
    #[must_use]
    pub(crate) fn fills(&self, work: &works::Model) -> bool {
        let missing = |stored: &Option<String>, offered: &Option<String>| {
            stored.is_none() && offered.is_some()
        };
        missing(&work.description, &self.description)
            || missing(&work.subjects, &self.subjects)
            || missing(&work.categories, &self.categories)
            || missing(&work.cover_url, &self.cover_url)
    }

    /// True when no field is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.subjects.is_none()
            && self.categories.is_none()
            && self.cover_url.is_none()
    }
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
    fn edition_ids_are_derived_from_the_listing_key() {
        let key = |plugin: &str, market: &str, product: &str| resolve::listing::ListingKey {
            plugin_key: plugin.into(),
            marketplace: market.into(),
            product_id: product.into(),
        };
        let id = edition_id_for_listing(&key("audible", "us", "B001"));
        assert_eq!(id, edition_id_for_listing(&key("audible", "us", "B001")));
        assert_eq!(Uuid::parse_str(&id).unwrap().get_version_num(), 5);
        for other in [
            key("audible", "uk", "B001"),
            key("libro", "us", "B001"),
            key("audible", "us", "B002"),
            // The separator keeps fields from running together.
            key("audibleu", "s", "B001"),
        ] {
            assert_ne!(id, edition_id_for_listing(&other), "{other:?}");
        }
    }

    #[test]
    fn a_library_id_is_eight_crockford_digits_of_the_edition_id() {
        assert_eq!(
            library_id("00000000-0000-4000-8000-000000000000").as_deref(),
            Some("00000000")
        );
        // 0xffffffffff is all ones: the last digit of the alphabet.
        assert_eq!(
            library_id("ffffffff-ff00-4000-8000-000000000000").as_deref(),
            Some("zzzzzzzz")
        );
        // 0x0123456789 in five-bit groups.
        assert_eq!(
            library_id("01234567-8900-4000-8000-000000000000").as_deref(),
            Some("04hmasw9")
        );
        let id = library_id(&Uuid::new_v4().to_string()).unwrap();
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|b| CROCKFORD.contains(&b)), "{id}");
        assert_eq!(library_id("edition-12").as_deref(), Some("edition1"));
        assert_eq!(library_id("--"), None);
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
    use crate::entities::{
        credits, holdings, resolution_conflicts, resolution_overrides, series_entries,
    };
    use crate::{InProcessSqliteAtomic, LibraryStore};
    use sea_orm::PaginatorTrait;
    use std::sync::Arc;

    /// A store over a fresh in-memory library, with the test accounts.
    async fn store() -> LibraryStore {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        seed_accounts(&store).await;
        store
    }

    /// The accounts the tests' holdings name, on every test plugin: a
    /// holding is only written for an account the library has.
    async fn seed_accounts(store: &LibraryStore) {
        for plugin in ["audible", "libro", "chirp"] {
            for id in ["a1", "a2", "c1", "l1"] {
                store
                    .upsert_account(plugin, id, "us", None, true, plugin)
                    .await
                    .unwrap();
            }
        }
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

    /// A wish is owned only when the resolver puts it in a work the library
    /// holds: the sequel of an owned book is its own work.
    #[tokio::test]
    async fn a_title_query_is_owned_through_its_work_not_its_words() {
        let store = self::store().await;
        store
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut dune = crate::NewBook::minimal("B002V1OF70", "acct", "us", "Dune");
        dune.authors = Some("Frank Herbert".into());
        dune.asin = Some("B002V1OF70".into());
        dune.isbn = Some("9780593288368".into());
        dune.series = Some("Dune".into());
        dune.series_index = Some("1".into());
        store.upsert_book(&dune).await.unwrap();
        let herbert = Some("Frank Herbert");

        // The same ASIN, or the same title and author (a wish's ISBN alone
        // is a weak identifier, which never joins an edition whose
        // expression the query knows nothing about): owned.
        for query in [
            TitleQuery {
                title: "Dune",
                asin: Some("B002V1OF70"),
                ..TitleQuery::default()
            },
            TitleQuery {
                title: "Dune",
                authors: herbert,
                isbn: Some("978-0-593-28836-8"),
                ..TitleQuery::default()
            },
            TitleQuery {
                title: "Dune",
                authors: herbert,
                ..TitleQuery::default()
            },
        ] {
            assert!(
                store.owned_work_for(&query).await.unwrap().is_some(),
                "{query:?}"
            );
        }
        // The sequel, by title or by its own ISBN, is another work.
        for query in [
            TitleQuery {
                title: "Dune Messiah",
                authors: herbert,
                isbn: Some("9781427202376"),
                ..TitleQuery::default()
            },
            TitleQuery {
                title: "Children of Dune",
                authors: herbert,
                ..TitleQuery::default()
            },
        ] {
            assert!(
                store.owned_work_for(&query).await.unwrap().is_none(),
                "{query:?}"
            );
        }
        // A work nobody holds is not owned even when it exists.
        assert!(store
            .owned_work_for(&TitleQuery {
                title: "Project Hail Mary",
                authors: Some("Andy Weir"),
                ..TitleQuery::default()
            })
            .await
            .unwrap()
            .is_none());
        // A query that is not a valid listing (a title over the listing
        // limit, or a blank one) matches no work instead of failing.
        let long_title = "Dune ".repeat(1000);
        for title in [long_title.as_str(), "  "] {
            assert!(store
                .owned_work_for(&TitleQuery {
                    title,
                    authors: herbert,
                    ..TitleQuery::default()
                })
                .await
                .unwrap()
                .is_none());
        }
        // `work_for` names the work whether or not it is held, and nothing
        // for a title the library has never seen.
        let dune_work = store
            .work_for(&TitleQuery {
                title: "Dune",
                authors: herbert,
                ..TitleQuery::default()
            })
            .await
            .unwrap()
            .expect("an existing work");
        assert!(store
            .work_for(&TitleQuery {
                title: "Project Hail Mary",
                authors: Some("Andy Weir"),
                ..TitleQuery::default()
            })
            .await
            .unwrap()
            .is_none());

        // Two wishes for that work from two stores' identifiers are one
        // queue entry, and the entry carries the work.
        for (title, asin, isbn) in [
            ("Dune", Some("B002V1OF70"), None),
            ("Dune", None, Some("9781427201447")),
        ] {
            let wish = store
                .create_title_request(&crate::NewTitleRequest {
                    uuid: None,
                    identity_id: None,
                    title: title.into(),
                    authors: Some("Frank Herbert".into()),
                    asin: asin.map(str::to_string),
                    isbn: isbn.map(str::to_string),
                    notes: None,
                    status: crate::RequestStatus::Open,
                    resolved_book_uuid: None,
                    cover_url: None,
                })
                .await
                .unwrap();
            assert_eq!(wish.work_id, dune_work);
        }
        let queue = store.list_global_request_queue().await.unwrap();
        assert_eq!(queue.len(), 1, "{queue:?}");
        assert_eq!(queue[0].work_id, dune_work);
        assert!(
            !queue[0].work_key.starts_with("work:"),
            "clients match on the bibliographic key: {}",
            queue[0].work_key
        );
        // The operator wished once for the work: the second is the same wish.
        assert_eq!(queue[0].wish_count, 1);
    }

    #[tokio::test]
    async fn a_wish_for_a_new_book_makes_one_catalog_work_and_edition() {
        let store = self::store().await;
        let query = TitleQuery {
            title: "Project Hail Mary",
            authors: Some("Andy Weir"),
            asin: Some("B08GB58KD5"),
            isbn: None,
            series: Some("Standalone"),
            series_index: Some("1"),
        };
        let (work_id, edition) = store.work_for_wish(&query).await.unwrap();
        let edition = edition.expect("the ASIN made a catalog edition");
        // The same wish again, and a wish naming only the ASIN under a
        // different title, reach the same rows.
        assert_eq!(
            store.work_for_wish(&query).await.unwrap(),
            (work_id.clone(), Some(edition.clone()))
        );
        let by_asin = store
            .work_for_wish(&TitleQuery {
                title: "Hail Mary",
                asin: Some("b08gb58kd5"),
                ..TitleQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(by_asin.0, work_id);
        assert_eq!(works::Entity::find().all(&store.db).await.unwrap().len(), 1);
        assert_eq!(
            editions::Entity::find().all(&store.db).await.unwrap().len(),
            1
        );
        // The catalog work carries the wish's title, authors and series, and
        // the library holds nothing of it.
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.title, "Project Hail Mary");
        assert_eq!(work.credits, vec![String::from("Andy Weir")]);
        assert_eq!(work.series, vec![String::from("Standalone")]);
        assert!(store
            .book_uuids_for_work(&work_id)
            .await
            .unwrap()
            .is_empty());
        assert!(holdings::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .is_empty());
        assert!(store.owned_work_for(&query).await.unwrap().is_none());
        // A later scan of the book finds the catalog work by its title and
        // author, so the wish and the library title share it.
        let mut draft = dune("audible", "B08GB58KD5");
        draft.title = String::from("Project Hail Mary");
        draft.credits = vec![(String::from("Andy Weir"), resolve::CreditRole::Author, None)];
        draft.series.clear();
        draft.identifiers.clear();
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let plan = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert!(plan.new_work.is_none(), "{plan:?}");
        assert_eq!(
            store.work_for(&query).await.unwrap().as_deref(),
            Some(work_id.as_str())
        );
    }

    /// A wish read back takes its title and authors from its work and its
    /// identifiers from the edition it named, since it copies none of them.
    #[tokio::test]
    async fn a_wish_reads_its_title_and_identifiers_through_its_work() {
        let store = self::store().await;
        let wish = |title: &str, asin: Option<&str>, isbn: Option<&str>| crate::NewTitleRequest {
            uuid: None,
            identity_id: None,
            title: title.into(),
            authors: Some("Ursula K. Le Guin".into()),
            asin: asin.map(str::to_string),
            isbn: isbn.map(str::to_string),
            notes: None,
            status: crate::RequestStatus::Open,
            resolved_book_uuid: None,
            cover_url: None,
        };
        let by_isbn = store
            .create_title_request(&wish("The Dispossessed", None, Some("9780061054884")))
            .await
            .unwrap();
        assert_eq!(by_isbn.title, "The Dispossessed");
        assert_eq!(by_isbn.authors.as_deref(), Some("Ursula K. Le Guin"));
        assert_eq!(by_isbn.isbn.as_deref(), Some("9780061054884"));
        assert!(by_isbn.asin.is_none());
        assert!(by_isbn.edition_id.is_some());
        assert_eq!(by_isbn.work_key, "isbn:9780061054884");
        let by_asin = store
            .create_title_request(&wish("The Left Hand of Darkness", Some("B000FC1ICM"), None))
            .await
            .unwrap();
        assert_eq!(by_asin.asin.as_deref(), Some("B000FC1ICM"));
        // A title alone still names a work, with no edition to read from.
        let bare = store
            .create_title_request(&wish("The Lathe of Heaven", None, None))
            .await
            .unwrap();
        assert!(bare.asin.is_none() && bare.isbn.is_none());
        assert_eq!(bare.title, "The Lathe of Heaven");
        let listed = store.list_title_requests(None).await.unwrap();
        assert_eq!(listed.len(), 3, "{listed:?}");
    }

    #[tokio::test]
    async fn a_wish_needs_a_title() {
        let store = self::store().await;
        let err = store
            .work_for_wish(&TitleQuery {
                title: "  ",
                ..TitleQuery::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_taken_derived_edition_id_falls_back_to_a_random_one() {
        use crate::entities::store_listings;
        let store = self::store().await;
        let mut draft = dune("audible", "B001");
        draft.identifiers.clear();
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let first = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert_eq!(
            first.listing.edition_id,
            edition_id_for_listing(&listing.key)
        );
        // The listing leaves the library while its edition stays, so the
        // next resolve mints an edition whose derived id is taken.
        holdings::Entity::delete_many()
            .filter(holdings::Column::ListingId.eq(first.listing.id.clone()))
            .exec(&store.db)
            .await
            .unwrap();
        store_listings::Entity::delete_by_id(first.listing.id.clone())
            .exec(&store.db)
            .await
            .unwrap();
        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        let minted = again.new_edition.expect("a new edition");
        assert_ne!(minted.id, first.listing.edition_id);
        assert_eq!(Uuid::parse_str(&minted.id).unwrap().get_version_num(), 4);
    }

    #[tokio::test]
    async fn only_a_live_editions_file_holds_a_name() {
        use crate::{FileOwner, FileRole};
        let store = self::store().await;
        let dune = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        let linked = store.record_listing(&dune, &account("a1")).await.unwrap();
        let held = FileOwner::Edition(linked.listing.edition_id.clone());
        let mine = FileOwner::Edition("another-edition".into());
        let record = |key: &'static str, owner: FileOwner| {
            let store = store.clone();
            async move {
                assert!(store
                    .record_edition_file("local", key, &owner, FileRole::Audio)
                    .await
                    .unwrap());
            }
        };
        record("A/Live.m4b", held.clone()).await;
        record("A/Gone.m4b", FileOwner::Edition("removed-edition".into())).await;
        record("A/Branch.m4b", FileOwner::BranchTitle("shared".into())).await;
        assert!(store
            .name_held_by_another_owner("A/Live", &mine)
            .await
            .unwrap());
        assert!(!store
            .name_held_by_another_owner("A/Live", &held)
            .await
            .unwrap());
        for stale in ["A/Gone", "A/Branch"] {
            assert!(
                !store
                    .name_held_by_another_owner(stale, &mine)
                    .await
                    .unwrap(),
                "{stale}"
            );
        }
        // An acquire's record refuses a live edition's key and takes over
        // a stale holder's.
        assert!(!store
            .record_edition_file("local", "A/Live.m4b", &mine, FileRole::Audio)
            .await
            .unwrap());
        assert_eq!(
            store
                .edition_file("local", "A/Live.m4b")
                .await
                .unwrap()
                .unwrap()
                .owner,
            held
        );
        for stale in ["A/Gone.m4b", "A/Branch.m4b"] {
            assert!(
                store
                    .record_edition_file("local", stale, &mine, FileRole::Audio)
                    .await
                    .unwrap(),
                "{stale}"
            );
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
        // The edition's id comes from the listing that first named it, so
        // another library resolving the same listing first agrees on it.
        assert_eq!(
            first.listing.edition_id,
            edition_id_for_listing(&audible.key)
        );
        let elsewhere = self::store().await;
        let theirs = elsewhere
            .record_listing(&audible, &account("someone-else"))
            .await
            .unwrap();
        assert_eq!(theirs.listing.edition_id, first.listing.edition_id);

        let db = &store.db;
        assert_eq!(holdings::Entity::find().count(db).await.unwrap(), 3);
        assert_eq!(works::Entity::find().count(db).await.unwrap(), 1);
        // Author at work, narrator at expression; written once each.
        assert_eq!(credits::Entity::find().count(db).await.unwrap(), 2);
        assert_eq!(series_entries::Entity::find().count(db).await.unwrap(), 1);
        let works = store.list_works().await.unwrap();
        assert_eq!(works[0].title, "Dune");
        assert_eq!(works[0].credits, ["Frank Herbert"]);
        assert_eq!(works[0].narrators, ["Scott Brick"]);
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
    async fn works_take_their_titles_metadata_where_they_have_none() {
        let store = store().await;
        store
            .upsert_account("audible", "a1", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00X", "a1", "US", "Dune");
        book.authors = Some("Frank Herbert".into());
        book.categories = Some("Science Fiction".into());
        let record = store.upsert_book(&book).await.unwrap();
        let work_id = store.work_id_for_book(&record.uuid).await.unwrap().unwrap();
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.categories.as_deref(), Some("Science Fiction"));
        assert!(work.description.is_none());

        store
            .update_catalog_enrichment(
                &record.uuid,
                &crate::CatalogEnrichmentFields {
                    description: Some("Spice.".into()),
                    cover_url: Some("https://covers.example/dune.jpg".into()),
                    ..crate::CatalogEnrichmentFields::default()
                },
            )
            .await
            .unwrap();
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.description.as_deref(), Some("Spice."));
        assert_eq!(
            work.cover_url.as_deref(),
            Some("https://covers.example/dune.jpg")
        );

        // A work that already has a value keeps it.
        store
            .update_catalog_enrichment(
                &record.uuid,
                &crate::CatalogEnrichmentFields {
                    description: Some("Other.".into()),
                    ..crate::CatalogEnrichmentFields::default()
                },
            )
            .await
            .unwrap();
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.description.as_deref(), Some("Spice."));
    }

    #[tokio::test]
    async fn listing_links_and_credit_batches_run_under_every_profile() {
        use crate::test_support::Profile;
        for profile in Profile::ALL {
            let store = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            let mut draft = dune("audible", "B001");
            draft.credits = (0..40)
                .map(|i| (format!("Cast Member {i}"), resolve::CreditRole::Cast, None))
                .collect();
            let listing = resolve::Listing::from_draft(&draft).unwrap();
            store
                .record_listing(&listing, &account("a1"))
                .await
                .unwrap();
            let written = credits::Entity::find().count(&store.db).await.unwrap();
            assert_eq!(written, 40, "{profile:?}");
        }
    }

    #[tokio::test]
    async fn a_credit_write_that_stopped_part_way_is_finished_next_scan() {
        let store = store().await;
        let mut draft = dune("audible", "B001");
        draft.credits = (0..40)
            .map(|i| (format!("Cast Member {i}"), resolve::CreditRole::Cast, None))
            .collect();
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let holder = resolve::Holder::Account("a1".into());
        let snapshot = store.listing_snapshot(&listing, &holder).await.unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert_eq!(
            store
                .run_link_listing(link_params(&listing, &plan, &snapshot))
                .await
                .unwrap(),
            atomic_status::OK
        );
        let batches = credit_batches(&plan);
        assert!(batches.len() > 1);
        // The first-billed credit is in the last batch.
        assert!(batches
            .last()
            .unwrap()
            .credits
            .iter()
            .any(|c| c.position == 0));
        // Only the first batch commits before the writer stops.
        store
            .run_write_credits(batches.into_iter().next().unwrap())
            .await
            .unwrap();
        let partial = credits::Entity::find().count(&store.db).await.unwrap();
        assert!(partial < 40);

        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert!(!again.credits.is_empty());
        assert_eq!(credits::Entity::find().count(&store.db).await.unwrap(), 40);
        assert!(store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap()
            .is_noop());
    }

    #[tokio::test]
    async fn an_episode_stored_before_its_show_gains_it() {
        let store = store().await;
        let show = resolve::ListingDraft {
            product_id: "S1".into(),
            title: "Show".into(),
            work_kind: Some(resolve::WorkKind::PodcastShow),
            expression_kind: Some(resolve::ExpressionKind::PodcastAudio),
            media_kind: Some(resolve::MediaKind::PodcastAudio),
            identifiers: Vec::new(),
            series: Vec::new(),
            ..dune("audible", "S1")
        };
        let episode = resolve::ListingDraft {
            product_id: "E1".into(),
            title: "Trailer".into(),
            work_kind: Some(resolve::WorkKind::PodcastEpisode),
            parent_product_id: Some("S1".into()),
            ..show.clone()
        };
        let episode = resolve::Listing::from_draft(&episode).unwrap();
        let first = store
            .record_listing(&episode, &account("a1"))
            .await
            .unwrap();
        let episode_work = first.new_work.unwrap().id;
        let show_plan = store
            .record_listing(
                &resolve::Listing::from_draft(&show).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let linked = store
            .record_listing(&episode, &account("a1"))
            .await
            .unwrap();
        assert!(linked.work_parent.is_some());
        let work = works::Entity::find_by_id(episode_work)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.parent_work_id, Some(show_plan.new_work.unwrap().id));
    }

    /// The page form of [`an_episode_stored_before_its_show_gains_it`]: the
    /// show first stored on a page is the episode's parent on the same page,
    /// though the page's read-ahead saw no show.
    #[tokio::test]
    async fn an_episode_gains_a_show_stored_earlier_on_its_page() {
        let store = store().await;
        let show = resolve::ListingDraft {
            product_id: "S1".into(),
            title: "Show".into(),
            work_kind: Some(resolve::WorkKind::PodcastShow),
            expression_kind: Some(resolve::ExpressionKind::PodcastAudio),
            media_kind: Some(resolve::MediaKind::PodcastAudio),
            identifiers: Vec::new(),
            series: Vec::new(),
            ..dune("audible", "S1")
        };
        let episode = resolve::ListingDraft {
            product_id: "E1".into(),
            title: "Trailer".into(),
            work_kind: Some(resolve::WorkKind::PodcastEpisode),
            parent_product_id: Some("S1".into()),
            ..show.clone()
        };
        let episode = resolve::Listing::from_draft(&episode).unwrap();
        let episode_work = store
            .record_listing(&episode, &account("a1"))
            .await
            .unwrap()
            .new_work
            .unwrap()
            .id;
        let show = resolve::Listing::from_draft(&show).unwrap();
        let items = vec![
            (show, account("a1"), WorkEnrichment::default()),
            (episode, account("a1"), WorkEnrichment::default()),
        ];
        store.record_listing_page(&items).await;
        let show_work = works::Entity::find()
            .filter(works::Column::Title.eq("Show"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .id;
        let work = works::Entity::find_by_id(episode_work)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.parent_work_id, Some(show_work));
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
            .with_atomic_txn(Arc::new(InProcessSqliteAtomic::on(db)));
        seed_accounts(&store).await;
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

    #[tokio::test]
    async fn a_relink_planned_from_a_stale_listing_answers_retry() {
        let store = store().await;
        let first = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("audible", "B001")).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let mut ebook = dune("audible", "B002");
        ebook.media_kind = Some(resolve::MediaKind::Ebook);
        ebook.identifiers.clear();
        let other = store
            .record_listing(
                &resolve::Listing::from_draft(&ebook).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let listing = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        let snapshot = store
            .listing_snapshot(&listing, &resolve::Holder::Account("a1".into()))
            .await
            .unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        let mut params = link_params(&listing, &plan, &snapshot);
        assert_eq!(
            params.stored_edition_id.as_deref(),
            Some(first.listing.edition_id.as_str())
        );
        params.edition_id.clone_from(&other.listing.edition_id);

        // Another writer moved the listing since the snapshot was read.
        let mut stale = params.clone();
        stale.stored_edition_id = Some("edition-read-before-the-race".into());
        assert_eq!(
            store.run_link_listing(stale).await.unwrap(),
            atomic_status::RETRY
        );
        let row = store_listings::Entity::find_by_id(first.listing.id.clone())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.edition_id, first.listing.edition_id);

        assert_eq!(
            store.run_link_listing(params).await.unwrap(),
            atomic_status::OK
        );
        let row = store_listings::Entity::find_by_id(first.listing.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.edition_id, other.listing.edition_id);
    }

    #[tokio::test]
    async fn a_weak_join_answers_retry_when_another_writer_gave_the_edition_another_asin() {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let store = store().await;
        // An ISBN-only edition.
        let isbn_only = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("libro", "L1")).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let edition = isbn_only.listing.edition_id.clone();
        // A listing with the ISBN and an ASIN plans to join it weakly.
        let mut with_asin = dune("audible", "B000000001");
        with_asin
            .identifiers
            .push((resolve::IdentifierScheme::Asin, "B000000001".into(), None));
        let listing = resolve::Listing::from_draft(&with_asin).unwrap();
        let holder = resolve::Holder::Account("a1".into());
        let snapshot = store.listing_snapshot(&listing, &holder).await.unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert_eq!(plan.link, resolve::LinkReason::WeakIdentifier);
        assert_eq!(plan.listing.edition_id, edition);

        // Meanwhile another writer attaches a different ASIN to it.
        let other =
            resolve::IdentifierKey::new(resolve::IdentifierScheme::Asin, "B000000002", None, "us")
                .unwrap();
        edition_identifiers::ActiveModel {
            edition_id: Set(edition.clone()),
            scheme: Set(other.scheme.as_str().into()),
            value: Set(other.value.clone()),
            marketplace: Set(other.marketplace.clone()),
            strength: Set(other.strength().as_str().into()),
            strong_key: Set(other.strong_key()),
            created_at: Set(now_str()),
        }
        .insert(&store.db)
        .await
        .unwrap();

        assert_eq!(
            store
                .run_link_listing(link_params(&listing, &plan, &snapshot))
                .await
                .unwrap(),
            atomic_status::RETRY
        );
        let asins_on_edition = edition_identifiers::Entity::find()
            .filter(edition_identifiers::Column::EditionId.eq(edition.as_str()))
            .filter(edition_identifiers::Column::Scheme.eq("asin"))
            .count(&store.db)
            .await
            .unwrap();
        assert_eq!(asins_on_edition, 1);

        // Resolving again keeps the two ASINs apart.
        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert_ne!(again.listing.edition_id, edition);
    }

    #[tokio::test]
    async fn a_weak_join_answers_retry_when_another_edition_claimed_its_asin() {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let store = store().await;
        let isbn_only = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("libro", "L1")).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let mut elsewhere = dune("audible", "B000000009");
        elsewhere.identifiers.clear();
        elsewhere.abridgement = Some(resolve::Abridgement::Abridged);
        let elsewhere = store
            .record_listing(
                &resolve::Listing::from_draft(&elsewhere).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        assert_ne!(elsewhere.listing.edition_id, isbn_only.listing.edition_id);

        let mut with_asin = dune("audible", "B000000001");
        with_asin
            .identifiers
            .push((resolve::IdentifierScheme::Asin, "B000000001".into(), None));
        let listing = resolve::Listing::from_draft(&with_asin).unwrap();
        let holder = resolve::Holder::Account("a1".into());
        let snapshot = store.listing_snapshot(&listing, &holder).await.unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert_eq!(plan.listing.edition_id, isbn_only.listing.edition_id);

        // Meanwhile the ASIN is claimed by the other edition.
        let asin = listing
            .edition_identifiers()
            .find(|k| k.scheme == resolve::IdentifierScheme::Asin)
            .unwrap()
            .clone();
        edition_identifiers::ActiveModel {
            edition_id: Set(elsewhere.listing.edition_id.clone()),
            scheme: Set(asin.scheme.as_str().into()),
            value: Set(asin.value.clone()),
            marketplace: Set(asin.marketplace.clone()),
            strength: Set(asin.strength().as_str().into()),
            strong_key: Set(asin.strong_key()),
            created_at: Set(now_str()),
        }
        .insert(&store.db)
        .await
        .unwrap();

        assert_eq!(
            store
                .run_link_listing(link_params(&listing, &plan, &snapshot))
                .await
                .unwrap(),
            atomic_status::RETRY
        );
        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert_eq!(again.link, resolve::LinkReason::StrongIdentifier);
        assert_eq!(again.listing.edition_id, elsewhere.listing.edition_id);
    }

    #[tokio::test]
    async fn identifiers_stay_off_an_edition_when_another_writer_stored_the_listing() {
        let store = store().await;
        let isbn_only = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("libro", "L1")).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();

        let mut with_asin = dune("audible", "B000000001");
        with_asin
            .identifiers
            .push((resolve::IdentifierScheme::Asin, "B000000001".into(), None));
        let listing = resolve::Listing::from_draft(&with_asin).unwrap();
        let holder = resolve::Holder::Account("a1".into());
        let snapshot = store.listing_snapshot(&listing, &holder).await.unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert_eq!(plan.listing.edition_id, isbn_only.listing.edition_id);

        // Meanwhile another scan stores the same product, described
        // differently, on an edition of its own.
        let mut other = dune("audible", "B000000001");
        other.identifiers.clear();
        other.abridgement = Some(resolve::Abridgement::Abridged);
        let other = store
            .record_listing(
                &resolve::Listing::from_draft(&other).unwrap(),
                &account("a2"),
            )
            .await
            .unwrap();
        assert_ne!(other.listing.edition_id, isbn_only.listing.edition_id);

        assert_eq!(
            store
                .run_link_listing(link_params(&listing, &plan, &snapshot))
                .await
                .unwrap(),
            atomic_status::RETRY
        );
        let asins = edition_identifiers::Entity::find()
            .filter(edition_identifiers::Column::Scheme.eq("asin"))
            .all(&store.db)
            .await
            .unwrap();
        assert!(asins.is_empty(), "{asins:?}");
    }

    #[tokio::test]
    async fn a_new_work_answers_retry_when_another_writer_stored_the_same_book() {
        let store = store().await;
        let mut draft = dune("audible", "B000000001");
        draft.identifiers = vec![(resolve::IdentifierScheme::Asin, "B000000001".into(), None)];
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let holder = resolve::Holder::Account("a1".into());
        let snapshot = store.listing_snapshot(&listing, &holder).await.unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert!(plan.new_work.is_some());

        // Meanwhile another store's listing of the same book is stored.
        let mut other = dune("libro", "L1");
        other.identifiers.clear();
        let other = store
            .record_listing(
                &resolve::Listing::from_draft(&other).unwrap(),
                &account("a2"),
            )
            .await
            .unwrap();
        let other_work = other.new_work.unwrap().id;

        assert_eq!(
            store
                .run_link_listing(link_params(&listing, &plan, &snapshot))
                .await
                .unwrap(),
            atomic_status::RETRY
        );
        assert_eq!(works::Entity::find().all(&store.db).await.unwrap().len(), 1);
        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert_eq!(again.work_reason, Some(resolve::WorkReason::Matcher));
        assert_eq!(works::Entity::find().all(&store.db).await.unwrap().len(), 1);
        let rows = works::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(rows[0].id, other_work);
    }

    #[tokio::test]
    async fn a_stale_plan_does_not_fill_an_edition_its_listing_left() {
        let store = store().await;
        let first = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("audible", "B001")).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let mut fuller = dune("audible", "B001");
        fuller.subtitle = Some("Book One".into());
        let listing = resolve::Listing::from_draft(&fuller).unwrap();
        let snapshot = store
            .listing_snapshot(&listing, &resolve::Holder::Account("a1".into()))
            .await
            .unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert!(plan.edition_update.is_some());
        // The plan believes the listing sold another edition, as it would
        // after a concurrent relink; its fill must not land.
        let mut stale = link_params(&listing, &plan, &snapshot);
        stale.stored_edition_id = Some("edition-read-before-the-race".into());
        store.run_link_listing(stale).await.unwrap();
        let edition = editions::Entity::find_by_id(first.listing.edition_id.clone())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edition.subtitle, None);
    }

    #[tokio::test]
    async fn a_new_work_answers_retry_when_another_work_claimed_its_work_identifier() {
        let store = store().await;
        let mut other = dune("audible", "B000000002");
        other.title = "Dune Messiah".into();
        other.identifiers = vec![(resolve::IdentifierScheme::Asin, "B000000002".into(), None)];
        let other = store
            .record_listing(
                &resolve::Listing::from_draft(&other).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let other_work = other.new_work.unwrap().id;

        let mut draft = dune("audible", "B000000001");
        draft.identifiers = vec![
            (resolve::IdentifierScheme::Asin, "B000000001".into(), None),
            (
                resolve::IdentifierScheme::OpenLibraryWork,
                "OL1W".into(),
                None,
            ),
        ];
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let holder = resolve::Holder::Account("a1".into());
        let snapshot = store.listing_snapshot(&listing, &holder).await.unwrap();
        let plan = resolve::resolve(&listing, &account("a1"), &snapshot, &mut UuidIds::default());
        assert!(plan.new_work.is_some());
        assert_eq!(plan.work_identifiers.len(), 1);

        // Meanwhile another writer gives the other work the same work ID.
        store
            .add_work_identifier(
                &other_work,
                resolve::IdentifierScheme::OpenLibraryWork,
                "OL1W",
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .run_link_listing(link_params(&listing, &plan, &snapshot))
                .await
                .unwrap(),
            atomic_status::RETRY
        );
        assert_eq!(works::Entity::find().all(&store.db).await.unwrap().len(), 1);
        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert!(again.new_work.is_none());
        let rows = work_identifiers::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].work_id, other_work);
        assert_eq!(works::Entity::find().all(&store.db).await.unwrap().len(), 1);
    }

    #[test]
    fn lookups_leave_room_under_the_bind_cap() {
        assert_eq!(lookup_chunk(100), LOOKUP_CHUNK);
        assert_eq!(lookup_chunk(32), 24);
        assert_eq!(lookup_chunk(1), 1);
        assert_eq!(lookup_chunk(u32::MAX), LOOKUP_CHUNK);
    }

    #[tokio::test]
    async fn a_work_with_more_expressions_than_one_lookup_binds_still_resolves() {
        let store = store()
            .await
            .with_max_binds(bookclerk_plugin_abi::HOST_MIN_BINDS);
        let chunk = store.lookup_chunk();
        let mut work = None;
        for i in 0..=chunk + 2 {
            let mut draft = dune("libro", &format!("L{i}"));
            draft.identifiers.clear();
            draft.credits = vec![
                ("Frank Herbert".into(), resolve::CreditRole::Author, None),
                (format!("Narrator {i}"), resolve::CreditRole::Narrator, None),
            ];
            let plan = store
                .record_listing(
                    &resolve::Listing::from_draft(&draft).unwrap(),
                    &account("a1"),
                )
                .await
                .unwrap();
            assert!(plan.new_expression.is_some());
            let id = plan.new_work.map(|w| w.id);
            if work.is_none() {
                work = id;
            } else {
                assert!(id.is_none());
            }
        }
        let expressions = expressions::Entity::find().all(&store.db).await.unwrap();
        assert!(expressions.len() > chunk);
        assert_eq!(works::Entity::find().all(&store.db).await.unwrap().len(), 1);
        let again = resolve::Listing::from_draft(&{
            let mut draft = dune("libro", "L0");
            draft.identifiers.clear();
            draft.credits = vec![
                ("Frank Herbert".into(), resolve::CreditRole::Author, None),
                ("Narrator 0".into(), resolve::CreditRole::Narrator, None),
            ];
            draft
        })
        .unwrap();
        assert!(store
            .record_listing(&again, &account("a1"))
            .await
            .unwrap()
            .is_noop());
    }

    #[tokio::test]
    async fn a_work_stored_without_an_author_gains_its_match_key() {
        let store = store().await;
        let mut anonymous = dune("audible", "B001");
        anonymous.identifiers.clear();
        anonymous.credits.clear();
        let first = store
            .record_listing(
                &resolve::Listing::from_draft(&anonymous).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let work = first.new_work.unwrap().id;

        let mut named = dune("audible", "B001");
        named.identifiers.clear();
        let named = store
            .record_listing(
                &resolve::Listing::from_draft(&named).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        assert!(named.work_match_key.is_some());
        let stored = works::Entity::find_by_id(work.clone())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.match_key, "dune\u{1f}frank herbert");

        let mut other = dune("libro", "L1");
        other.identifiers.clear();
        let other = store
            .record_listing(
                &resolve::Listing::from_draft(&other).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        assert_eq!(other.work_reason, Some(resolve::WorkReason::Matcher));
        assert!(other.new_work.is_none());
    }

    #[tokio::test]
    async fn a_rescan_fills_what_the_expression_did_not_know() {
        let store = store().await;
        let mut bare = dune("libro", "L1");
        bare.credits.truncate(1);
        bare.language = None;
        bare.abridgement = None;
        let first = store
            .record_listing(
                &resolve::Listing::from_draft(&bare).unwrap(),
                &account("l1"),
            )
            .await
            .unwrap();
        let expression = first.new_expression.unwrap().id;

        let full = resolve::Listing::from_draft(&dune("libro", "L1")).unwrap();
        let filled = store.record_listing(&full, &account("l1")).await.unwrap();
        assert!(filled.expression_update.is_some());
        let stored = expressions::Entity::find_by_id(expression)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.language.as_deref(), Some("en"));
        assert_eq!(stored.abridgement, "unabridged");
        assert_eq!(stored.credit_key, "narrator:scott brick");

        // Another store's listing with the same ISBN now joins the edition.
        let other = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("chirp", "C1")).unwrap(),
                &account("c1"),
            )
            .await
            .unwrap();
        assert_eq!(other.link, resolve::LinkReason::WeakIdentifier);
        assert_eq!(other.listing.edition_id, first.listing.edition_id);
    }

    #[tokio::test]
    async fn a_rescan_refreshes_changed_store_metadata() {
        let store = store().await;
        let listing = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        let first = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        let mut renamed = dune("audible", "B001");
        renamed.title = "Dune (Unabridged)".into();
        renamed.categories = Some("Science Fiction".into());
        let renamed = resolve::Listing::from_draft(&renamed).unwrap();
        store
            .record_listing(&renamed, &account("a1"))
            .await
            .unwrap();
        let row = store_listings::Entity::find_by_id(first.listing.id.clone())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.title, "Dune (Unabridged)");
        assert_eq!(row.categories.as_deref(), Some("Science Fiction"));

        // An unchanged rescan only writes once `observed_at` is a day old.
        let now = Utc::now();
        assert!(!listing_needs_refresh(&row, &renamed, now));
        assert!(listing_needs_refresh(
            &row,
            &renamed,
            now + chrono::TimeDelta::hours(25)
        ));
        let unreadable = store_listings::Model {
            observed_at: "not a time".into(),
            ..row
        };
        assert!(listing_needs_refresh(&unreadable, &renamed, now));
    }

    #[tokio::test]
    async fn a_pin_to_a_removed_edition_does_not_block_the_listing() {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let store = store().await;
        let listing = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        let first = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        resolution_overrides::ActiveModel {
            subject_kind: Set("listing".into()),
            subject_id: Set(first.listing.id.clone()),
            action: Set("pin_edition".into()),
            target_id: Set(Some("edition-that-was-removed".into())),
            created_by_user_id: Set(None),
            created_at: Set(now_str()),
        }
        .insert(&store.db)
        .await
        .unwrap();
        let again = store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        assert_eq!(again.listing.edition_id, first.listing.edition_id);
    }

    #[tokio::test]
    async fn a_rescan_updates_the_holding() {
        let store = store().await;
        let listing = resolve::Listing::from_draft(&dune("audible", "B001")).unwrap();
        store
            .record_listing(&listing, &account("a1"))
            .await
            .unwrap();
        let dated = resolve::HoldingDraft {
            purchased_at: Some("2024-01-01T00:00:00+00:00".into()),
            ..account("a1")
        };
        let plan = store.record_listing(&listing, &dated).await.unwrap();
        assert!(plan.holding_changed);
        let row = holdings::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.purchased_at.as_deref(), dated.purchased_at.as_deref());
    }

    #[tokio::test]
    async fn work_identifiers_keep_their_first_work() {
        let store = store().await;
        let plan = store
            .record_listing(
                &resolve::Listing::from_draft(&dune("audible", "B001")).unwrap(),
                &account("a1"),
            )
            .await
            .unwrap();
        let work = plan.new_work.unwrap().id;
        for (target, raw) in [
            (work.as_str(), "OL1W"),
            ("another-work", "OL1W"),
            (work.as_str(), "OL1W"),
        ] {
            store
                .add_work_identifier(target, resolve::IdentifierScheme::OpenLibraryWork, raw)
                .await
                .unwrap();
        }
        // An edition-level scheme is not a work identifier.
        store
            .add_work_identifier(&work, resolve::IdentifierScheme::Asin, "B001")
            .await
            .unwrap();
        let rows = work_identifiers::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].work_id, work);
    }

    #[test]
    fn a_works_series_are_written_in_one_batch() {
        let mut draft = dune("audible", "B001");
        draft.credits = (0..20)
            .map(|i| (format!("Cast Member {i}"), resolve::CreditRole::Cast, None))
            .collect();
        draft.series = (0..resolve::listing::MAX_SERIES)
            .map(|i| (format!("Series {i}"), Some(i.to_string())))
            .collect();
        let listing = resolve::Listing::from_draft(&draft).unwrap();
        let plan = resolve::resolve(
            &listing,
            &account("a1"),
            &resolve::Snapshot::default(),
            &mut UuidIds::default(),
        );
        let batches = credit_batches(&plan);
        let with_series: Vec<_> = batches
            .iter()
            .filter(|b| !b.series_entries.is_empty())
            .collect();
        assert_eq!(with_series.len(), 1);
        assert_eq!(
            with_series[0].series_entries.len(),
            resolve::listing::MAX_SERIES
        );
        assert!(batches
            .iter()
            .all(|b| b.statement_count() <= CREDIT_BATCH_STATEMENTS));
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
            &mut UuidIds::default(),
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

    /// A small library with shared editions, a pin, an isolated work, a
    /// work identifier and a podcast episode under its show.
    async fn seeded(store: &LibraryStore) -> Vec<resolve::Listing> {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let mut drafts = vec![dune("audible", "B001"), dune("libro", "L001")];
        let mut children = dune("audible", "B002");
        children.title = "Children of Dune".into();
        children.identifiers = vec![
            (
                resolve::IdentifierScheme::Isbn,
                "9780593098240".into(),
                None,
            ),
            (
                resolve::IdentifierScheme::OpenLibraryWork,
                "OL2W".into(),
                None,
            ),
        ];
        drafts.push(children);
        let mut show = dune("audible", "SHOW1");
        show.title = "Spice Talk".into();
        show.work_kind = Some(resolve::WorkKind::PodcastShow);
        show.identifiers.clear();
        show.series.clear();
        let mut episode = show.clone();
        episode.product_id = "EP1".into();
        episode.title = "Spice Talk 1".into();
        episode.work_kind = Some(resolve::WorkKind::PodcastEpisode);
        episode.parent_product_id = Some("SHOW1".into());
        drafts.push(show);
        drafts.push(episode);
        let listings: Vec<resolve::Listing> = drafts
            .iter()
            .map(|d| resolve::Listing::from_draft(d).unwrap())
            .collect();
        let mut plans = Vec::new();
        for listing in &listings {
            plans.push(store.record_listing(listing, &account("a1")).await.unwrap());
        }
        resolution_overrides::ActiveModel {
            subject_kind: Set("listing".into()),
            subject_id: Set(plans[1].listing.id.clone()),
            action: Set("pin_edition".into()),
            target_id: Set(Some(plans[1].listing.edition_id.clone())),
            created_by_user_id: Set(None),
            created_at: Set(now_str()),
        }
        .insert(&store.db)
        .await
        .unwrap();
        let children_work = store
            .works_of_editions(BTreeSet::from([plans[2].listing.edition_id.clone()]))
            .await
            .unwrap()
            .remove(&plans[2].listing.edition_id)
            .unwrap();
        resolution_overrides::ActiveModel {
            subject_kind: Set("work".into()),
            subject_id: Set(children_work),
            action: Set("isolate".into()),
            target_id: Set(None),
            created_by_user_id: Set(None),
            created_at: Set(now_str()),
        }
        .insert(&store.db)
        .await
        .unwrap();
        listings
    }

    #[tokio::test]
    async fn pooled_snapshots_match_per_listing_snapshots_under_every_profile() {
        use crate::test_support::Profile;
        for profile in Profile::ALL {
            let store = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            let mut listings = seeded(&store).await;
            // A listing the library has never seen, sharing Dune's ISBN.
            listings.push(resolve::Listing::from_draft(&dune("chirp", "C001")).unwrap());
            let mut stranger = dune("audible", "B999");
            stranger.title = "Something Else".into();
            stranger.identifiers.clear();
            listings.push(resolve::Listing::from_draft(&stranger).unwrap());

            let refs: Vec<&resolve::Listing> = listings.iter().collect();
            let holder = resolve::Holder::Account("a1".into());
            let pool = ListingPool::load(&store, &refs, &holder).await.unwrap();
            // Holders are compared by position so no account id reaches a
            // failure message.
            for (nth, holder) in [holder.clone(), resolve::Holder::Account("a2".into())]
                .into_iter()
                .enumerate()
            {
                for listing in &listings {
                    let direct = store
                        .listing_snapshot_and_row(listing, &holder)
                        .await
                        .unwrap();
                    let pooled = store
                        .listing_snapshot_from(
                            SnapshotReader {
                                store: &store,
                                pool: Some(&pool),
                            },
                            listing,
                            &holder,
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        pooled, direct,
                        "{profile:?}: {} for holder {nth}",
                        listing.key.product_id
                    );
                }
            }
        }
    }

    /// Row counts of the library model's tables, for comparing two stores.
    async fn model_counts(store: &LibraryStore) -> Vec<u64> {
        use crate::entities::store_listings;
        vec![
            works::Entity::find().count(&store.db).await.unwrap(),
            expressions::Entity::find().count(&store.db).await.unwrap(),
            editions::Entity::find().count(&store.db).await.unwrap(),
            store_listings::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            holdings::Entity::find().count(&store.db).await.unwrap(),
            credits::Entity::find().count(&store.db).await.unwrap(),
            series_entries::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            edition_identifiers::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            work_identifiers::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
        ]
    }

    #[tokio::test]
    async fn a_page_writes_what_listing_by_listing_writes_under_every_profile() {
        use crate::test_support::Profile;
        for profile in Profile::ALL {
            let by_listing = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            let by_page = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            seed_accounts(&by_listing).await;
            seed_accounts(&by_page).await;
            let mut listings = seeded(&by_listing).await;
            seeded(&by_page).await;
            listings.push(resolve::Listing::from_draft(&dune("chirp", "C001")).unwrap());
            let mut renamed = dune("audible", "B001");
            renamed.title = "Dune (Movie Tie-In)".into();
            listings.push(resolve::Listing::from_draft(&renamed).unwrap());
            let dated = resolve::HoldingDraft {
                purchased_at: Some("2024-01-01T00:00:00+00:00".into()),
                ..account("a1")
            };
            let fill = WorkEnrichment {
                description: Some("Spice.".into()),
                ..WorkEnrichment::default()
            };

            let items: Vec<(resolve::Listing, resolve::HoldingDraft, WorkEnrichment)> = listings
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let holding = if i == 1 { dated.clone() } else { account("a1") };
                    (l.clone(), holding, fill.clone())
                })
                .collect();
            for (listing, holding, fill) in &items {
                let plan = by_listing.record_listing(listing, holding).await.unwrap();
                by_listing
                    .fill_work_from_book(&plan.listing.edition_id, fill)
                    .await
                    .unwrap();
            }
            by_page.record_listing_page(&items).await;

            assert_eq!(
                model_counts(&by_page).await,
                model_counts(&by_listing).await,
                "{profile:?}"
            );
            let described = |store: &LibraryStore| {
                let db = store.db.clone();
                async move {
                    works::Entity::find()
                        .filter(works::Column::Description.is_not_null())
                        .count(&db)
                        .await
                        .unwrap()
                }
            };
            assert_eq!(described(&by_page).await, described(&by_listing).await);
            let purchased = holdings::Entity::find()
                .filter(holdings::Column::PurchasedAt.is_not_null())
                .count(&by_page.db)
                .await
                .unwrap();
            assert_eq!(purchased, 1, "{profile:?}: the changed holding is written");

            // A second pass over the same page changes nothing.
            let before = model_counts(&by_page).await;
            by_page.record_listing_page(&items).await;
            assert_eq!(model_counts(&by_page).await, before, "{profile:?}");
        }
    }

    #[tokio::test]
    async fn a_title_listed_twice_in_a_page_ends_as_its_last_listing_under_every_profile() {
        use crate::entities::store_listings;
        use crate::test_support::Profile;
        for profile in Profile::ALL {
            let store = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            let listings = seeded(&store).await;
            let original = listings
                .iter()
                .find(|l| l.key.product_id == "B001")
                .unwrap()
                .clone();
            let mut renamed = dune("audible", "B001");
            renamed.title = "Dune (Movie Tie-In)".into();
            let renamed = resolve::Listing::from_draft(&renamed).unwrap();
            // The renamed listing writes first; the original then settles
            // against a pool read before that write, which must not count.
            let items = vec![
                (renamed, account("a1"), WorkEnrichment::default()),
                (original, account("a1"), WorkEnrichment::default()),
            ];
            store.record_listing_page(&items).await;
            let row = store_listings::Entity::find()
                .filter(store_listings::Column::ProductId.eq("B001"))
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.title, items[1].0.title, "{profile:?}");
        }
    }
    /// Lookups are sized on the request the store's reads send (the proxy's
    /// longer operation id and the stamped hash), so a lookup that fits only
    /// a shorter request is narrowed rather than refused at the proxy.
    #[tokio::test]
    async fn lookups_are_sized_on_the_request_a_read_sends() {
        use crate::entities::store_listings;
        use std::sync::Mutex;
        let store = store().await;
        let ids: Vec<String> = (0..4).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        for id in &ids {
            let listing = resolve::Listing::from_draft(&dune("audible", id)).unwrap();
            store
                .record_listing(&listing, &account("a1"))
                .await
                .unwrap();
        }
        let query = |part: &[String]| {
            store_listings::Entity::find()
                .filter(store_listings::Column::ProductId.is_in(part.iter().cloned()))
        };
        let stmt = crate::control_plane::sea_exec(&query(&ids)).unwrap();
        let short = crate::control_plane::batch::request("live.admit", vec![stmt.clone()]);
        let short_bytes = bookclerk_plugin_abi::encoded_execute_request_bytes(&short)
            .unwrap()
            .len();
        let mut caps = store.db_capabilities().clone();
        caps.max_request_bytes = u32::try_from(short_bytes).unwrap();
        let store = store.with_db_capabilities(caps);
        // Fits the short request exactly, but not the one a read sends.
        assert!(store.admits_statement(&stmt));
        assert!(!store.admits_read(&stmt));
        let admits = |part: &[String]| {
            store.admits_read(&crate::control_plane::sea_exec(&query(part)).unwrap())
        };
        assert!(admits(&ids[..1]), "one id fits the read");
        // Every call starting at the same id is a probe until the last,
        // which is the statement sent.
        let calls = Mutex::new(Vec::<(String, usize)>::new());
        let rows = find_chunked::<store_listings::Entity, _>(&store, &ids, |part| {
            calls.lock().unwrap().push((part[0].clone(), part.len()));
            query(part)
        })
        .await
        .unwrap();
        let mut sent = std::collections::BTreeMap::new();
        for (first, len) in calls.into_inner().unwrap() {
            let at = ids.iter().position(|id| *id == first).unwrap();
            sent.insert(at, len);
        }
        assert!(sent.len() > 1, "the lookup is narrowed: {sent:?}");
        let mut next = 0;
        for (at, len) in sent {
            assert_eq!(at, next, "chunks cover the ids in order");
            assert!(admits(&ids[at..at + len]), "{len} ids at {at}");
            next = at + len;
        }
        assert_eq!(next, ids.len());
        let mut found: Vec<String> = rows.into_iter().map(|row| row.product_id).collect();
        found.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(found, want, "every seeded row is read once");
    }

    /// A chunk whose reply is refused as over a result limit is read again
    /// in halves until each read fits, and every key is still read once.
    #[tokio::test]
    async fn a_chunk_refused_for_its_reply_size_is_read_in_halves() {
        use std::sync::Mutex;
        let keys: Vec<u32> = (0..10).collect();
        let sizes = Mutex::new(Vec::new());
        let rows = read_in_chunks(
            &keys,
            8,
            |_| true,
            |part: &[u32]| {
                sizes.lock().unwrap().push(part.len());
                let part = part.to_vec();
                async move {
                    if part.len() > 3 {
                        let limit = bookclerk_plugin_abi::LimitExceeded::new(
                            "maxResultRows",
                            3,
                            u64::try_from(part.len()).unwrap(),
                        );
                        Err(sea_orm::DbErr::Custom(limit.to_message()))
                    } else {
                        Ok(part)
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(rows, keys);
        assert_eq!(sizes.into_inner().unwrap(), vec![8, 4, 2, 2, 2, 2, 2]);

        // A single key still over the limit, or any other failure, is returned.
        let err = read_in_chunks(
            &keys[..1],
            8,
            |_| true,
            |_: &[u32]| async {
                Err::<Vec<u32>, _>(sea_orm::DbErr::Custom(
                    bookclerk_plugin_abi::LimitExceeded::new("maxResultBytes", 1, 2).to_message(),
                ))
            },
        )
        .await;
        assert!(err.is_err());
    }

    /// A chunked lookup of long ids sends only statements the backend
    /// admits: on D1 a full-width lookup of UUIDs is past `maxPayloadBytes`,
    /// so the chunks narrow, and every id is still read.
    #[tokio::test]
    async fn a_chunked_lookup_of_uuids_fits_the_payload_cap_under_every_profile() {
        use crate::entities::store_listings;
        use crate::test_support::Profile;
        use std::sync::Mutex;
        for profile in Profile::ALL {
            let store = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            let ids: Vec<String> = (0..200).map(|_| uuid::Uuid::new_v4().to_string()).collect();
            let query = |part: &[String]| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::ProductId.is_in(part.iter().cloned()))
            };
            let admits = |part: &[String]| {
                store.admits_read(&crate::control_plane::sea_exec(&query(part)).unwrap())
            };
            let full = store.lookup_chunk();
            if profile == Profile::D1 {
                assert!(!admits(&ids[..full]), "a full-width UUID lookup fits D1");
            }
            // Every call starting at the same id is a probe until the last,
            // which is the statement sent.
            let calls = Mutex::new(Vec::<(String, usize)>::new());
            let rows = find_chunked::<store_listings::Entity, _>(&store, &ids, |part| {
                calls.lock().unwrap().push((part[0].clone(), part.len()));
                query(part)
            })
            .await
            .unwrap();
            assert!(rows.is_empty(), "{profile:?}");
            let mut sent = std::collections::BTreeMap::new();
            for (first, len) in calls.into_inner().unwrap() {
                let at = ids.iter().position(|id| *id == first).unwrap();
                sent.insert(at, len);
            }
            let mut next = 0;
            for (at, len) in sent {
                assert_eq!(at, next, "{profile:?}: chunks cover the ids in order");
                assert!(admits(&ids[at..at + len]), "{profile:?}: {len} ids at {at}");
                next = at + len;
            }
            assert_eq!(next, ids.len(), "{profile:?}");
        }
    }

    /// A page of long product ids (UUIDs, as some storefronts use) fits the
    /// payload cap of every profile: a D1 statement carries about 3.6 KB, so
    /// a lookup sized from the bind cap alone would be refused.
    #[tokio::test]
    async fn a_page_of_uuid_product_ids_records_under_every_profile() {
        use crate::entities::store_listings;
        use crate::test_support::Profile;
        use sea_orm::PaginatorTrait;
        for profile in Profile::ALL {
            let store = crate::test_support::sqlite::memory(profile)
                .await
                .into_store();
            let items: Vec<_> = (0..120)
                .map(|n| {
                    let mut draft = dune("libro", &uuid::Uuid::new_v4().to_string());
                    draft.title = format!("Title {n}");
                    draft.identifiers.clear();
                    (
                        resolve::Listing::from_draft(&draft).unwrap(),
                        account("a1"),
                        WorkEnrichment::default(),
                    )
                })
                .collect();
            store.record_listing_page(&items).await;
            let stored = store_listings::Entity::find()
                .count(&store.db)
                .await
                .unwrap();
            assert_eq!(stored, 120, "{profile:?}");
            // A second pass reads every listing back through the pool.
            store.record_listing_page(&items).await;
            let stored = store_listings::Entity::find()
                .count(&store.db)
                .await
                .unwrap();
            assert_eq!(stored, 120, "{profile:?}");
        }
    }
}
