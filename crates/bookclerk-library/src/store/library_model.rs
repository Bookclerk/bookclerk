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
    LinkEvent, LinkExpressionFill, LinkExpressionRow, LinkIdentifierRow, LinkWorkIdentifierRow,
    LinkWorkMatchKey, LinkWorkParent, LinkWorkRow, ListingLinkParams, SeriesEntryRow, SeriesRow,
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

/// Rows of `E` that `query` selects for each chunk of at most `chunk`
/// of `ids`, so no lookup binds more than the backend accepts.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a read fails.
async fn find_chunked<E, F>(
    db: &sea_orm::DatabaseConnection,
    chunk: usize,
    ids: &[String],
    query: F,
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
    F: Fn(&[String]) -> sea_orm::Select<E>,
{
    let mut out = Vec::new();
    for part in ids.chunks(chunk.max(1)) {
        out.extend(query(part).all(db).await.map_err(LibraryError::Orm)?);
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
fn listing_from_query(query: &TitleQuery<'_>) -> Result<resolve::Listing> {
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
    resolve::Listing::from_draft(&draft)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("title query: {err}")))
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
    fn lookup_chunk(&self) -> usize {
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
    /// owned: *Dune Messiah* is its own work beside *Dune*.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the query cannot form a listing (blank
    /// title) or a read fails.
    pub async fn owned_work_for(&self, query: &TitleQuery<'_>) -> Result<Option<String>> {
        let listing = listing_from_query(query)?;
        let holder = resolve::Holder::Account(String::from("query"));
        let holding = resolve::HoldingDraft {
            holder: holder.clone(),
            entitlement: resolve::Entitlement::Purchase,
            purchased_at: None,
        };
        let snapshot = self.listing_snapshot(&listing, &holder).await?;
        let plan = resolve::resolve(&listing, &holding, &snapshot, &mut UuidIds::default());
        let Some(work_id) = planned_work_id(&plan, &snapshot) else {
            return Ok(None);
        };
        if self.book_uuids_for_work(&work_id).await?.is_empty() {
            return Ok(None);
        }
        Ok(Some(work_id))
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
        for _ in 0..LINK_ATTEMPTS {
            let (snapshot, stored_row) = self
                .listing_snapshot_and_row(listing, &holding.holder)
                .await?;
            if let Some(missing) = snapshot.is_closed() {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "library rows for listing {} reference a missing {missing}",
                    listing.key.product_id
                )));
            }
            let mut plan = resolve::resolve(listing, holding, &snapshot, &mut UuidIds::default());
            if let Some(edition) = &plan.new_edition {
                if self.edition_exists(&edition.id).await? {
                    plan = resolve::resolve(
                        listing,
                        holding,
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
        let db = &self.db;
        let chunk = self.lookup_chunk();
        let find_listing = |key: &resolve::ListingKey| {
            store_listings::Entity::find()
                .filter(store_listings::Column::PluginKey.eq(key.plugin_key.as_str()))
                .filter(store_listings::Column::Marketplace.eq(key.marketplace.as_str()))
                .filter(store_listings::Column::ProductId.eq(key.product_id.as_str()))
        };
        let stored_row = find_listing(&listing.key)
            .one(db)
            .await
            .map_err(LibraryError::Orm)?;
        let stored = stored_row.clone().map(listing_row);
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
        // Each key binds a scheme, a value and a marketplace.
        for part in keys.chunks((chunk / 3).max(1)) {
            let mut any = Condition::any();
            for key in part {
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

        let edition_list: Vec<String> = edition_ids.iter().cloned().collect();
        let mut editions_found = Vec::new();
        for m in find_chunked(db, chunk, &edition_list, |part| {
            editions::Entity::find().filter(editions::Column::Id.is_in(part.iter().cloned()))
        })
        .await?
        {
            editions_found.push(edition_row(m)?);
        }
        for m in find_chunked(db, chunk, &edition_list, |part| {
            edition_identifiers::Entity::find()
                .filter(edition_identifiers::Column::EditionId.is_in(part.iter().cloned()))
        })
        .await?
        {
            identifier_rows.insert(edition_identifier_row(m)?);
        }

        let mut work_identifier_rows = Vec::new();
        let work_keys: Vec<&resolve::IdentifierKey> = listing.work_identifiers().collect();
        // Each key binds a scheme and a value.
        for part in work_keys.chunks((chunk / 2).max(1)) {
            let mut any = Condition::any();
            for key in part {
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

        let expression_ids: Vec<String> = editions_found
            .iter()
            .map(|e| e.expression_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut expressions_found = Vec::new();
        for m in find_chunked(db, chunk, &expression_ids, |part| {
            expressions::Entity::find().filter(expressions::Column::Id.is_in(part.iter().cloned()))
        })
        .await?
        {
            expressions_found.push(expression_row(m)?);
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

        let work_list: Vec<String> = work_ids.iter().cloned().collect();
        let mut works_found = Vec::new();
        for m in find_chunked(db, chunk, &work_list, |part| {
            works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
        })
        .await?
        {
            works_found.push(work_row(&m)?);
        }
        for m in find_chunked(db, chunk, &work_list, |part| {
            expressions::Entity::find()
                .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
        })
        .await?
        {
            if !expressions_found.iter().any(|x| x.id == m.id) {
                expressions_found.push(expression_row(m)?);
            }
        }
        {
            let isolated = find_chunked(db, chunk, &work_list, |part| {
                resolution_overrides::Entity::find()
                    .filter(resolution_overrides::Column::SubjectKind.eq("work"))
                    .filter(resolution_overrides::Column::Action.eq("isolate"))
                    .filter(resolution_overrides::Column::SubjectId.is_in(part.iter().cloned()))
            })
            .await?;
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
            let firsts = find_chunked(db, chunk, ids, |part| {
                credits::Entity::find()
                    .filter(credits::Column::TargetKind.eq(level.as_str()))
                    .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                    .filter(credits::Column::Position.eq(0))
            })
            .await?;
            credited.extend(firsts.into_iter().map(|c| (level, c.target_id)));
        }
        let works_with_series: Vec<String> = find_chunked(db, chunk, &work_list, |part| {
            series_entries::Entity::find()
                .filter(series_entries::Column::WorkId.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(|e| e.work_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

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
    async fn works_of_editions(
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
        let chunk = self.lookup_chunk();
        let edition_ids: Vec<String> = find_chunked(&self.db, chunk, &expression_ids, |part| {
            editions::Entity::find()
                .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(|e| e.id)
        .collect();
        if edition_ids.is_empty() {
            return Ok(Vec::new());
        }
        let listings = find_chunked(&self.db, chunk, &edition_ids, |part| {
            store_listings::Entity::find()
                .filter(store_listings::Column::EditionId.is_in(part.iter().cloned()))
        })
        .await?;
        let keys: BTreeSet<(String, String, String)> = listings
            .into_iter()
            .map(|l| (l.plugin_key, l.marketplace, l.product_id))
            .collect();
        let product_ids: Vec<String> = keys
            .iter()
            .map(|(_, _, product)| product.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut uuids = BTreeSet::new();
        for chunk in product_ids.chunks(self.lookup_chunk()) {
            for b in books::Entity::find()
                .filter(books::Column::ProductId.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                let key = (
                    b.plugin_key,
                    resolve::stored_marketplace(&b.marketplace),
                    b.product_id,
                );
                if keys.contains(&key) {
                    uuids.insert(b.uuid);
                }
            }
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
        let missing = |stored: &Option<String>, offered: &Option<String>| {
            stored.is_none() && offered.is_some()
        };
        if !(missing(&work.description, &fill.description)
            || missing(&work.subjects, &fill.subjects)
            || missing(&work.categories, &fill.categories)
            || missing(&work.cover_url, &fill.cover_url))
        {
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
}
