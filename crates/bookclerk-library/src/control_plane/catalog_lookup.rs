//! Catalog lookup between libraries: a library asks the branches that
//! joined it whether they can supply works it knows, and records their
//! answers in its shared listing pool.
//!
//! Work ids are local to each library, so a question names the work by its
//! strong identifiers ([`bookclerk_node_proto::LookupWork`]): the edition
//! ASINs, ISBNs and OpenLibrary ids of the library's editions of it, and
//! its OpenLibrary work id. Titles are never compared on either side.
//!
//! On the library:
//!
//! 1. [`ask_branches`] stores one `catalog_lookups` row per trusted branch
//!    and work (a new question, or one whose answer is older than
//!    [`LOOKUP_ANSWER_TTL`]) and wakes each branch with a
//!    `catalog.asked` node message. The message is a hint; the rows are
//!    the state.
//! 2. The branch pulls them ([`pending_questions`], the `catalog.questions`
//!    frame), at most [`bookclerk_node_proto::MAX_LOOKUP_WORKS`] at a time.
//!    A question sent and not answered within [`LOOKUP_RESEND`] goes out
//!    again.
//! 3. [`record_answers`] (the `catalog.found` frame) stores each edition
//!    the branch can supply as a catalog listing from source
//!    `branch:<node>` ([`branch_listing_key`]): plugin key `branch:<node>`,
//!    product id `<store>:<product id>`. An edition is kept only when the
//!    resolver places it, through a shared identifier, on an edition this
//!    library already has of the asked work; anything else is dropped, so
//!    a branch cannot add works or editions here. Listings the branch named
//!    earlier for the same work and no longer names are expired. Search
//!    and Discover read them like any listing; [`branch_listings_for_works`]
//!    returns the current ones.
//!
//! On the branch, [`answer_questions`] maps each question to a work of its
//! own library by identifier (a strong identifier first; a weak ISBN only
//! when it names one work) and lists the books of that work its people
//! hold: the owner's always, and the members' while the link's "List my
//! members' books" setting is on and the member has not opted out
//! ([`set_catalog_listing_opt_out`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bookclerk_node_proto::{
    found_problem, Availability, CatalogMetadata, LookupAnswer, LookupEdition, LookupFormat,
    LookupIdentifier, LookupScheme, LookupWork, MAX_LOOKUP_EDITIONS, MAX_LOOKUP_IDENTIFIERS,
    MAX_LOOKUP_WORKS,
};
use bookclerk_plugin_abi::TypedDbStatement;
use bookclerk_resolve as resolve;
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::batch::{exec, request, rows_affected, text};
use super::enrollment::load_node_credential;
use super::node_queue::{notify_node, NodeMessage};
use crate::entities::{
    account_links, books, catalog_listing_optouts, catalog_lookups, edition_identifiers, editions,
    expressions, nodes, portal_identities, store_listings, users, work_identifiers,
};
use crate::error::{LibraryError, Result};
use crate::models::{AcquireStatus, UserStatus};
use crate::store::catalog_listings::{CatalogListing, ListingDetail};
use crate::store::LibraryStore;

/// How long a branch's answer stands: a question answered longer ago is
/// asked again the next time [`ask_branches`] names its work.
pub const LOOKUP_ANSWER_TTL: chrono::Duration = chrono::Duration::days(1);

/// How long a sent question waits for its answer before it is sent again.
pub const LOOKUP_RESEND: chrono::Duration = chrono::Duration::minutes(10);

/// How long a `branch:<node>` listing counts as current after the branch
/// last named it: a day of answer life plus a day for the next question to
/// be asked and answered.
pub const BRANCH_LISTING_TTL: chrono::Duration = chrono::Duration::days(2);

/// Plugin key prefix of the listings branches answer with.
pub const BRANCH_LISTING_PREFIX: &str = "branch:";

/// Ids per `IN (...)` read.
const READ_CHUNK: usize = 200;

/// Rows read per [`pending_questions`] call, so works with no identifiers
/// skipped along the way do not starve the batch.
const PENDING_SCAN: u64 = 400;

/// The plugin key of listings answered by branch `node_id`.
#[must_use]
pub fn branch_listing_key(node_id: &str) -> String {
    format!("{BRANCH_LISTING_PREFIX}{node_id}")
}

/// RFC 3339 now, with microseconds so stamps order as text.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Parses a stored RFC 3339 stamp.
fn parse(at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// True when `node_id` is a branch this library trusts: a `host` node with
/// a live credential and an active owner user.
async fn is_trusted_branch(store: &LibraryStore, row: &nodes::Model) -> Result<bool> {
    if row.kind != "host" {
        return Ok(false);
    }
    let Some(owner) = row.owner_user_id else {
        return Ok(false);
    };
    let live = load_node_credential(store, &row.node_id)
        .await?
        .is_some_and(|credential| credential.revoked_at.is_none());
    if !live {
        return Ok(false);
    }
    Ok(users::Entity::find_by_id(owner)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some_and(|user| UserStatus::parse(&user.status) == Some(UserStatus::Active)))
}

/// The branches this library trusts, by node id.
///
/// # Errors
///
/// Returns the read error.
pub async fn trusted_branches(store: &LibraryStore) -> Result<Vec<String>> {
    let rows = nodes::Entity::find()
        .filter(nodes::Column::Kind.eq("host"))
        .filter(nodes::Column::OwnerUserId.is_not_null())
        .order_by_asc(nodes::Column::NodeId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut out = Vec::new();
    for row in rows {
        if is_trusted_branch(store, &row).await? {
            out.push(row.node_id);
        }
    }
    Ok(out)
}

/// True when `node_id` is a branch this library trusts.
async fn trusts(store: &LibraryStore, node_id: &str) -> Result<bool> {
    match nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    {
        Some(row) => is_trusted_branch(store, &row).await,
        None => Ok(false),
    }
}

/// Runs `statements` as atomic batches sized to the database plugin.
///
/// # Returns
///
/// The rows the statements changed, summed.
async fn run_batches(
    store: &LibraryStore,
    what: &'static str,
    statements: Vec<TypedDbStatement>,
) -> Result<u64> {
    let mut batches = crate::store::write_batches::WriteBatches::new(store.db_capabilities(), what);
    let mut full = Vec::new();
    for statement in statements {
        if let Some(batch) = batches.push(statement)? {
            full.push(batch);
        }
    }
    full.extend(batches.finish());
    let mut changed = 0;
    for batch in full {
        let count = batch.len();
        let operation_id = format!("catalog-lookup-{}", uuid::Uuid::new_v4());
        let reply = store
            .execute_host_batch(request(&operation_id, batch))
            .await?;
        changed += (0..count).map(|i| rows_affected(&reply, i)).sum::<u64>();
    }
    Ok(changed)
}

/// What [`ask_branches`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskReport {
    /// Branches asked.
    pub branches: usize,
    /// Questions added or asked again, over every branch.
    pub asked: u64,
}

/// Asks every trusted branch about `work_ids`.
///
/// A work a branch was never asked about gets a new question; one whose
/// answer is older than [`LOOKUP_ANSWER_TTL`] is asked again; one waiting
/// for its answer, or answered recently, is left alone. A work id this
/// library does not have is skipped. Each branch with a new question gets a
/// `catalog.asked` wakeup.
///
/// # Arguments
///
/// * `work_ids` - Works of this library to ask about.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn ask_branches(store: &LibraryStore, work_ids: &[String]) -> Result<AskReport> {
    let works: BTreeSet<&String> = work_ids.iter().collect();
    let branches = trusted_branches(store).await?;
    let mut report = AskReport {
        branches: branches.len(),
        asked: 0,
    };
    if works.is_empty() {
        return Ok(report);
    }
    let now = Utc::now();
    let now_text = stamp(now);
    let stale = stamp(now - LOOKUP_ANSWER_TTL);
    for node_id in &branches {
        let mut statements = Vec::with_capacity(works.len() * 2);
        for work_id in &works {
            statements.push(exec(
                "UPDATE catalog_lookups SET asked_at = ?, sent_at = NULL, answered_at = NULL \
                 WHERE node_id = ? AND work_id = ? AND answered_at IS NOT NULL AND answered_at < ?",
                vec![text(&now_text), text(node_id), text(work_id), text(&stale)],
            ));
            statements.push(exec(
                "INSERT OR IGNORE INTO catalog_lookups (node_id, work_id, asked_at) \
                 SELECT ?, id, ? FROM works WHERE id = ?",
                vec![text(node_id), text(&now_text), text(work_id)],
            ));
        }
        let asked = run_batches(store, "catalog question", statements).await?;
        if asked > 0 {
            notify_node(store, node_id, NodeMessage::CatalogAsked {}).await;
        }
        report.asked += asked;
    }
    Ok(report)
}

/// Reads `ids` in chunks with `query`.
async fn read_chunked<E, F>(store: &LibraryStore, ids: &[String], query: F) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
    F: Fn(Vec<String>) -> sea_orm::Select<E>,
{
    let mut out = Vec::new();
    for part in ids.chunks(READ_CHUNK) {
        out.extend(
            query(part.to_vec())
                .all(store.db())
                .await
                .map_err(LibraryError::Orm)?,
        );
    }
    Ok(out)
}

/// The identifiers each of `work_ids` is asked about by: its editions'
/// ASINs, ISBNs and OpenLibrary edition ids and its OpenLibrary work id,
/// strong ones first, at most [`MAX_LOOKUP_IDENTIFIERS`] per work.
async fn identifiers_of(
    store: &LibraryStore,
    work_ids: &[String],
) -> Result<BTreeMap<String, Vec<LookupIdentifier>>> {
    let expressions = read_chunked(store, work_ids, |part| {
        expressions::Entity::find().filter(expressions::Column::WorkId.is_in(part))
    })
    .await?;
    let work_of_expression: HashMap<String, String> =
        expressions.into_iter().map(|x| (x.id, x.work_id)).collect();
    let expression_ids: Vec<String> = work_of_expression.keys().cloned().collect();
    let editions = read_chunked(store, &expression_ids, |part| {
        editions::Entity::find().filter(editions::Column::ExpressionId.is_in(part))
    })
    .await?;
    let work_of_edition: HashMap<String, String> = editions
        .into_iter()
        .filter_map(|e| {
            work_of_expression
                .get(&e.expression_id)
                .map(|w| (e.id, w.clone()))
        })
        .collect();
    let edition_ids: Vec<String> = work_of_edition.keys().cloned().collect();
    let mut found: BTreeMap<String, BTreeSet<(u8, LookupIdentifier)>> = BTreeMap::new();
    for row in read_chunked(store, &edition_ids, |part| {
        edition_identifiers::Entity::find()
            .filter(edition_identifiers::Column::EditionId.is_in(part))
    })
    .await?
    {
        let Some(scheme) = LookupScheme::parse(&row.scheme) else {
            continue;
        };
        let Some(work) = work_of_edition.get(&row.edition_id) else {
            continue;
        };
        let identifier = LookupIdentifier {
            scheme,
            value: row.value,
            marketplace: if scheme.is_scoped() {
                row.marketplace
            } else {
                String::new()
            },
        };
        let rank = u8::from(row.strength != "strong");
        found
            .entry(work.clone())
            .or_default()
            .insert((rank, identifier));
    }
    for row in read_chunked(store, work_ids, |part| {
        work_identifiers::Entity::find().filter(work_identifiers::Column::WorkId.is_in(part))
    })
    .await?
    {
        let Some(scheme) = LookupScheme::parse(&row.scheme) else {
            continue;
        };
        if scheme != LookupScheme::OpenlibraryWork {
            continue;
        }
        let identifier = LookupIdentifier {
            scheme,
            value: row.value,
            marketplace: String::new(),
        };
        found
            .entry(row.work_id)
            .or_default()
            .insert((0, identifier));
    }
    Ok(found
        .into_iter()
        .map(|(work, ids)| {
            let ids: Vec<LookupIdentifier> = ids
                .into_iter()
                .map(|(_, id)| id)
                .filter(LookupIdentifier::is_well_formed)
                .take(MAX_LOOKUP_IDENTIFIERS)
                .collect();
            (work, ids)
        })
        .filter(|(_, ids)| !ids.is_empty())
        .collect())
}

/// The questions waiting for branch `node_id`, marked sent.
///
/// A question is waiting while it has no answer and was never sent or was
/// sent more than [`LOOKUP_RESEND`] ago. A work with no identifier to ask
/// by is marked answered (there is nothing to ask), so it does not come
/// back until it is asked again.
///
/// # Returns
///
/// `None` when `node_id` is not a branch this library trusts; otherwise at
/// most [`MAX_LOOKUP_WORKS`] questions, oldest first.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn pending_questions(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Option<Vec<LookupWork>>> {
    if !trusts(store, node_id).await? {
        return Ok(None);
    }
    let now = Utc::now();
    let resend_before = now - LOOKUP_RESEND;
    let rows = catalog_lookups::Entity::find()
        .filter(catalog_lookups::Column::NodeId.eq(node_id))
        .filter(catalog_lookups::Column::AnsweredAt.is_null())
        .order_by_asc(catalog_lookups::Column::AskedAt)
        .order_by_asc(catalog_lookups::Column::WorkId)
        .limit(PENDING_SCAN)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let due: Vec<String> = rows
        .into_iter()
        .filter(|row| {
            row.sent_at
                .as_deref()
                .and_then(parse)
                .is_none_or(|sent| sent < resend_before)
        })
        .map(|row| row.work_id)
        .collect();
    if due.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let identifiers = identifiers_of(store, &due).await?;
    let mut questions = Vec::new();
    let mut nothing_to_ask = Vec::new();
    for work_id in due {
        match identifiers.get(&work_id) {
            Some(ids) if bookclerk_node_proto::is_work_ref(&work_id) => {
                if questions.len() < MAX_LOOKUP_WORKS {
                    questions.push(LookupWork {
                        work: work_id,
                        identifiers: ids.clone(),
                    });
                }
            }
            _ => nothing_to_ask.push(work_id),
        }
    }
    let now_text = stamp(now);
    let mut statements = Vec::new();
    for question in &questions {
        statements.push(exec(
            "UPDATE catalog_lookups SET sent_at = ? WHERE node_id = ? AND work_id = ? \
             AND answered_at IS NULL",
            vec![text(&now_text), text(node_id), text(&question.work)],
        ));
    }
    for work_id in &nothing_to_ask {
        statements.push(exec(
            "UPDATE catalog_lookups SET answered_at = ? WHERE node_id = ? AND work_id = ? \
             AND answered_at IS NULL",
            vec![text(&now_text), text(node_id), text(work_id)],
        ));
    }
    run_batches(store, "catalog question", statements).await?;
    Ok(Some(questions))
}

/// Outcome of [`record_answers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswersRecorded {
    /// The answers were applied.
    Recorded(AnswerReport),
    /// The node is not a branch this library trusts.
    Refused(&'static str),
}

/// What [`record_answers`] stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerReport {
    /// Works the answer settled (answered or "not here").
    pub works: u64,
    /// Branch listings stored or refreshed.
    pub listings: u64,
    /// Editions dropped: for a work not asked of this branch, or one the
    /// resolver did not place on a stored edition of the asked work.
    pub dropped: u64,
    /// Earlier branch listings the answer no longer names, now expired.
    pub expired: u64,
}

/// The catalog listing an answered edition is stored as.
fn edition_listing(node_id: &str, edition: &LookupEdition) -> CatalogListing {
    let meta = &edition.metadata;
    let marketplace = (!edition.marketplace.is_empty()).then(|| edition.marketplace.clone());
    let mut identifiers = Vec::new();
    if let Some(asin) = meta.asin.as_deref().filter(|a| !a.trim().is_empty()) {
        identifiers.push((
            resolve::IdentifierScheme::Asin,
            asin.to_string(),
            marketplace.clone(),
        ));
    }
    if let Some(isbn) = meta.isbn.as_deref().filter(|i| !i.trim().is_empty()) {
        identifiers.push((resolve::IdentifierScheme::Isbn, isbn.to_string(), None));
    }
    let mut credits = Vec::new();
    for (names, role) in [
        (&meta.authors, resolve::CreditRole::Author),
        (&meta.narrators, resolve::CreditRole::Narrator),
    ] {
        if let Some(names) = names.as_deref() {
            credits.extend(
                resolve::names::split_display_names(names)
                    .into_iter()
                    .map(|name| (name, role, None)),
            );
        }
    }
    let episode = meta
        .content_kind
        .as_deref()
        .is_some_and(crate::models::is_episode);
    let (work_kind, expression_kind, media_kind) = if episode {
        (
            resolve::WorkKind::PodcastEpisode,
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
    CatalogListing {
        draft: resolve::ListingDraft {
            plugin_key: branch_listing_key(node_id),
            marketplace,
            product_id: format!("{}:{}", edition.store, edition.product_id),
            work_kind: Some(work_kind),
            expression_kind: Some(expression_kind),
            media_kind: Some(media_kind),
            language: meta.language.clone(),
            abridgement: None,
            title: meta.title.clone(),
            subtitle: meta.subtitle.clone(),
            published_at: meta.published_at.clone(),
            duration_minutes: meta.length_minutes,
            categories: None,
            identifiers,
            credits,
            series: meta
                .series
                .iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| (s.clone(), meta.series_index.clone()))
                .collect(),
            parent_product_id: None,
        },
        detail: ListingDetail {
            subtitle: meta.subtitle.clone(),
            authors: meta.authors.clone(),
            narrators: meta.narrators.clone(),
            series: meta.series.clone(),
            series_index: meta.series_index.clone(),
            publisher: meta.publisher.clone(),
            published_at: meta.published_at.clone(),
            length_minutes: meta.length_minutes,
            language: meta.language.clone(),
            ..ListingDetail::default()
        },
        offers: Vec::new(),
    }
}

/// Records branch `node_id`'s answers to the questions it was sent.
///
/// Only works with a question waiting for this branch are settled; an
/// answer for any other work is dropped. Each answered edition is stored
/// as a `branch:<node>` catalog listing when the resolver places it on a
/// stored edition of the asked work (through a shared identifier, never a
/// title); other editions are dropped. Branch listings of a settled work
/// the answer no longer names are expired, and the settled questions are
/// marked answered.
///
/// # Arguments
///
/// * `answers` - Works the branch can supply.
/// * `missing` - Works it looked up and cannot supply.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a malformed answer, and a read or
/// write error.
pub async fn record_answers(
    store: &LibraryStore,
    node_id: &str,
    answers: &[LookupAnswer],
    missing: &[String],
) -> Result<AnswersRecorded> {
    if let Some(problem) = found_problem(answers, missing) {
        return Err(LibraryError::Invalid(format!("catalog answer: {problem}")));
    }
    if !trusts(store, node_id).await? {
        return Ok(AnswersRecorded::Refused("not_a_branch"));
    }
    let named: Vec<String> = answers
        .iter()
        .map(|a| a.work.clone())
        .chain(missing.iter().cloned())
        .collect();
    let waiting: BTreeSet<String> = read_chunked(store, &named, |part| {
        catalog_lookups::Entity::find()
            .filter(catalog_lookups::Column::NodeId.eq(node_id))
            .filter(catalog_lookups::Column::WorkId.is_in(part))
            .filter(catalog_lookups::Column::AnsweredAt.is_null())
    })
    .await?
    .into_iter()
    .map(|row| row.work_id)
    .collect();
    let mut report = AnswerReport::default();
    let mut listings = Vec::new();
    let mut listing_work = Vec::new();
    for answer in answers {
        if !waiting.contains(&answer.work) {
            report.dropped += u64::try_from(answer.editions.len()).unwrap_or(u64::MAX);
            continue;
        }
        for edition in answer.editions.iter().take(MAX_LOOKUP_EDITIONS) {
            let listing = edition_listing(node_id, edition);
            let placed = match resolve::Listing::from_draft(&listing.draft) {
                Ok(parsed) => store.existing_edition_work(&parsed).await?,
                Err(_) => None,
            };
            if placed.as_deref() == Some(answer.work.as_str()) {
                listings.push(listing);
                listing_work.push(answer.work.clone());
            } else {
                report.dropped += 1;
            }
        }
    }
    let recorded = store.record_catalog_listings(&listings).await?;
    let mut kept: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, work) in recorded.ids.iter().zip(&listing_work) {
        if let Some(id) = id {
            report.listings += 1;
            kept.entry(work.clone()).or_default().insert(id.clone());
        }
    }
    let settled: Vec<String> = waiting.iter().cloned().collect();
    let now_text = stamp(Utc::now());
    let mut statements = Vec::new();
    for listing in branch_listings_of_works(store, node_id, &settled).await? {
        let still = kept
            .get(&listing.work_id)
            .is_some_and(|ids| ids.contains(&listing.listing_id));
        if !still {
            statements.push(exec(
                "UPDATE store_listings SET observed_at = ? WHERE id = ?",
                vec![text(EXPIRED_STAMP), text(&listing.listing_id)],
            ));
            report.expired += 1;
        }
    }
    for work_id in &settled {
        statements.push(exec(
            "UPDATE catalog_lookups SET answered_at = ? WHERE node_id = ? AND work_id = ? \
             AND answered_at IS NULL",
            vec![text(&now_text), text(node_id), text(work_id)],
        ));
    }
    run_batches(store, "catalog answer", statements).await?;
    report.works = u64::try_from(settled.len()).unwrap_or(u64::MAX);
    Ok(AnswersRecorded::Recorded(report))
}

/// `observed_at` of an expired branch listing: older than any TTL, so
/// readers skip it and the catalog prune removes it once nothing refers
/// to it.
const EXPIRED_STAMP: &str = "1970-01-01T00:00:00Z";

/// A `branch:<node>` listing and the work its edition belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchListing {
    /// `store_listings.id`.
    pub listing_id: String,
    /// The branch node it is available through.
    pub node_id: String,
    /// The work it supplies.
    pub work_id: String,
    /// The store the branch has it from (`audible`).
    pub store: String,
    /// The store's product id.
    pub product_id: String,
    /// When the branch last named it.
    pub observed_at: DateTime<Utc>,
}

/// The works of `edition_ids`, by edition.
async fn works_of_editions(
    store: &LibraryStore,
    edition_ids: &[String],
) -> Result<HashMap<String, String>> {
    let editions = read_chunked(store, edition_ids, |part| {
        editions::Entity::find().filter(editions::Column::Id.is_in(part))
    })
    .await?;
    let expression_ids: Vec<String> = editions
        .iter()
        .map(|e| e.expression_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let work_of_expression: HashMap<String, String> =
        read_chunked(store, &expression_ids, |part| {
            expressions::Entity::find().filter(expressions::Column::Id.is_in(part))
        })
        .await?
        .into_iter()
        .map(|x| (x.id, x.work_id))
        .collect();
    Ok(editions
        .into_iter()
        .filter_map(|e| {
            work_of_expression
                .get(&e.expression_id)
                .map(|w| (e.id, w.clone()))
        })
        .collect())
}

/// The editions of `work_ids`, with their work.
async fn editions_of_works(
    store: &LibraryStore,
    work_ids: &[String],
) -> Result<HashMap<String, String>> {
    let work_of_expression: HashMap<String, String> = read_chunked(store, work_ids, |part| {
        expressions::Entity::find().filter(expressions::Column::WorkId.is_in(part))
    })
    .await?
    .into_iter()
    .map(|x| (x.id, x.work_id))
    .collect();
    let expression_ids: Vec<String> = work_of_expression.keys().cloned().collect();
    Ok(read_chunked(store, &expression_ids, |part| {
        editions::Entity::find().filter(editions::Column::ExpressionId.is_in(part))
    })
    .await?
    .into_iter()
    .filter_map(|e| {
        work_of_expression
            .get(&e.expression_id)
            .map(|w| (e.id, w.clone()))
    })
    .collect())
}

/// Every `branch:<node>` listing (current or not) of `work_ids` from
/// `node_id`.
async fn branch_listings_of_works(
    store: &LibraryStore,
    node_id: &str,
    work_ids: &[String],
) -> Result<Vec<BranchListing>> {
    let key = branch_listing_key(node_id);
    Ok(branch_listings(store, work_ids, None)
        .await?
        .into_iter()
        .filter(|l| branch_listing_key(&l.node_id) == key)
        .collect())
}

/// The `branch:<node>` listings of `work_ids`, optionally only those named
/// since `since`.
async fn branch_listings(
    store: &LibraryStore,
    work_ids: &[String],
    since: Option<DateTime<Utc>>,
) -> Result<Vec<BranchListing>> {
    let editions = editions_of_works(store, work_ids).await?;
    let edition_ids: Vec<String> = editions.keys().cloned().collect();
    let rows = read_chunked(store, &edition_ids, |part| {
        store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.is_in(part))
            .filter(store_listings::Column::PluginKey.starts_with(BRANCH_LISTING_PREFIX))
    })
    .await?;
    let mut out = Vec::new();
    for row in rows {
        let Some(node_id) = row.plugin_key.strip_prefix(BRANCH_LISTING_PREFIX) else {
            continue;
        };
        let Some(observed_at) = parse(&row.observed_at) else {
            continue;
        };
        if since.is_some_and(|since| observed_at < since) {
            continue;
        }
        let Some(work_id) = editions.get(&row.edition_id) else {
            continue;
        };
        let (store_id, product_id) = row
            .product_id
            .split_once(':')
            .map_or((String::new(), row.product_id.clone()), |(s, p)| {
                (s.to_string(), p.to_string())
            });
        out.push(BranchListing {
            listing_id: row.id,
            node_id: node_id.to_string(),
            work_id: work_id.clone(),
            store: store_id,
            product_id,
            observed_at,
        });
    }
    out.sort_by(|a, b| {
        (&a.work_id, &a.node_id, &a.listing_id).cmp(&(&b.work_id, &b.node_id, &b.listing_id))
    });
    Ok(out)
}

/// The current branch listings of `work_ids`: those a branch this library
/// still trusts named within [`BRANCH_LISTING_TTL`].
///
/// # Errors
///
/// Returns the read error.
pub async fn branch_listings_for_works(
    store: &LibraryStore,
    work_ids: &[String],
) -> Result<Vec<BranchListing>> {
    let trusted: BTreeSet<String> = trusted_branches(store).await?.into_iter().collect();
    Ok(
        branch_listings(store, work_ids, Some(Utc::now() - BRANCH_LISTING_TTL))
            .await?
            .into_iter()
            .filter(|l| trusted.contains(&l.node_id))
            .collect(),
    )
}

/// The works this library wants its branches asked about: those of open
/// wishes and of Discover candidates, at most `limit`.
///
/// # Errors
///
/// Returns the read error.
pub async fn works_of_interest(store: &LibraryStore, limit: u64) -> Result<Vec<String>> {
    use crate::entities::{discover_candidates, title_requests};
    let mut works: BTreeSet<String> = title_requests::Entity::find()
        .filter(title_requests::Column::Status.eq("open"))
        .limit(limit)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|r| r.work_id)
        .collect();
    let listing_ids: Vec<String> = discover_candidates::Entity::find()
        .limit(limit)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|c| c.listing_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let edition_ids: Vec<String> = read_chunked(store, &listing_ids, |part| {
        store_listings::Entity::find().filter(store_listings::Column::Id.is_in(part))
    })
    .await?
    .into_iter()
    .map(|l| l.edition_id)
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect();
    works.extend(works_of_editions(store, &edition_ids).await?.into_values());
    Ok(works
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect())
}

// ---------------------------------------------------------------------------
// On the branch.

/// Records whether member `user_id`'s books are kept out of what this
/// branch lists to the libraries it joined.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_catalog_listing_opt_out(
    store: &LibraryStore,
    user_id: i64,
    opted_out: bool,
) -> Result<()> {
    let statement = if opted_out {
        exec(
            "INSERT OR IGNORE INTO catalog_listing_optouts (user_id, created_at) VALUES (?, ?)",
            vec![super::batch::int(user_id), text(&stamp(Utc::now()))],
        )
    } else {
        exec(
            "DELETE FROM catalog_listing_optouts WHERE user_id = ?",
            vec![super::batch::int(user_id)],
        )
    };
    run_batches(store, "catalog listing opt-out", vec![statement]).await?;
    Ok(())
}

/// True when member `user_id` keeps their books out of this branch's
/// listings.
///
/// # Errors
///
/// Returns the read error.
pub async fn catalog_listing_opted_out(store: &LibraryStore, user_id: i64) -> Result<bool> {
    Ok(catalog_listing_optouts::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some())
}

/// The work of this library a question names, by identifier: the one work
/// its strong identifiers (ASIN, OpenLibrary edition or work id) reach,
/// or, with none, the one work its ISBNs reach. `None` when no work, or
/// more than one, is reached.
async fn work_for_question(store: &LibraryStore, question: &LookupWork) -> Result<Option<String>> {
    let mut strong_editions = BTreeSet::new();
    let mut weak_editions = BTreeSet::new();
    let mut strong_works = BTreeSet::new();
    for id in &question.identifiers {
        match id.scheme {
            LookupScheme::OpenlibraryWork => {
                if let Some(row) = work_identifiers::Entity::find()
                    .filter(work_identifiers::Column::Scheme.eq(id.scheme.as_str()))
                    .filter(work_identifiers::Column::Value.eq(id.value.as_str()))
                    .one(store.db())
                    .await
                    .map_err(LibraryError::Orm)?
                {
                    strong_works.insert(row.work_id);
                }
            }
            LookupScheme::Asin | LookupScheme::Isbn | LookupScheme::OpenlibraryEdition => {
                let rows = edition_identifiers::Entity::find()
                    .filter(edition_identifiers::Column::Scheme.eq(id.scheme.as_str()))
                    .filter(edition_identifiers::Column::Value.eq(id.value.as_str()))
                    .filter(edition_identifiers::Column::Marketplace.eq(id.marketplace.as_str()))
                    .all(store.db())
                    .await
                    .map_err(LibraryError::Orm)?;
                for row in rows {
                    if row.strength == "strong" {
                        strong_editions.insert(row.edition_id);
                    } else {
                        weak_editions.insert(row.edition_id);
                    }
                }
            }
        }
    }
    let strong: Vec<String> = strong_editions.into_iter().collect();
    strong_works.extend(works_of_editions(store, &strong).await?.into_values());
    let works = if strong_works.is_empty() {
        let weak: Vec<String> = weak_editions.into_iter().collect();
        works_of_editions(store, &weak)
            .await?
            .into_values()
            .collect::<BTreeSet<_>>()
    } else {
        strong_works
    };
    if works.len() == 1 {
        Ok(works.into_iter().next())
    } else {
        Ok(None)
    }
}

/// Who may have their books listed: the users linked to each account,
/// whether each is an Owner, and who opted out.
struct Listers {
    /// Users linked to each `(plugin key, account id)`.
    users_of: HashMap<(String, String), Vec<i64>>,
    /// Active Owners.
    owners: BTreeSet<i64>,
    /// Active users.
    active: BTreeSet<i64>,
    /// Members who opted out.
    opted_out: BTreeSet<i64>,
}

impl Listers {
    /// Reads who holds the accounts of `books`.
    async fn load(store: &LibraryStore, books: &[books::Model]) -> Result<Self> {
        let account_ids: Vec<String> = books
            .iter()
            .map(|b| b.account_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let links = read_chunked(store, &account_ids, |part| {
            account_links::Entity::find().filter(account_links::Column::AccountId.is_in(part))
        })
        .await?;
        let identity_ids: Vec<i64> = links.iter().map(|l| l.identity_id).collect();
        let mut user_of_identity = HashMap::new();
        for part in identity_ids.chunks(READ_CHUNK) {
            for identity in portal_identities::Entity::find()
                .filter(portal_identities::Column::Id.is_in(part.to_vec()))
                .all(store.db())
                .await
                .map_err(LibraryError::Orm)?
            {
                if let Some(user) = identity.user_id {
                    user_of_identity.insert(identity.id, user);
                }
            }
        }
        let mut users_of: HashMap<(String, String), Vec<i64>> = HashMap::new();
        for link in links {
            if let Some(user) = user_of_identity.get(&link.identity_id) {
                users_of
                    .entry((link.plugin_key, link.account_id))
                    .or_default()
                    .push(*user);
            }
        }
        let user_ids: Vec<i64> = users_of
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut owners = BTreeSet::new();
        let mut active = BTreeSet::new();
        let mut opted_out = BTreeSet::new();
        for part in user_ids.chunks(READ_CHUNK) {
            for user in users::Entity::find()
                .filter(users::Column::Id.is_in(part.to_vec()))
                .all(store.db())
                .await
                .map_err(LibraryError::Orm)?
            {
                if UserStatus::parse(&user.status) == Some(UserStatus::Active) {
                    active.insert(user.id);
                    if user.role == "owner" {
                        owners.insert(user.id);
                    }
                }
            }
            for row in catalog_listing_optouts::Entity::find()
                .filter(catalog_listing_optouts::Column::UserId.is_in(part.to_vec()))
                .all(store.db())
                .await
                .map_err(LibraryError::Orm)?
            {
                opted_out.insert(row.user_id);
            }
        }
        Ok(Self {
            users_of,
            owners,
            active,
            opted_out,
        })
    }

    /// True when `book` may be listed: its account belongs to no user (the
    /// install's own), to an Owner, or, with `members` on, to an active
    /// member who has not opted out.
    fn may_list(&self, book: &books::Model, members: bool) -> bool {
        let Some(users) = self
            .users_of
            .get(&(book.plugin_key.clone(), book.account_id.clone()))
        else {
            return true;
        };
        users.iter().any(|u| self.owners.contains(u))
            || (members
                && users
                    .iter()
                    .any(|u| self.active.contains(u) && !self.opted_out.contains(u)))
    }
}

/// The answer edition for `book`, when its store, product id and title
/// pass the frame rules.
fn book_edition(book: &books::Model) -> Option<LookupEdition> {
    let mut formats = Vec::new();
    if AcquireStatus::parse(&book.acquire_status) == Some(AcquireStatus::Acquired) {
        formats.push(LookupFormat::Audio);
    }
    if AcquireStatus::parse(&book.pdf_status) == Some(AcquireStatus::Acquired) {
        formats.push(LookupFormat::Pdf);
    }
    let edition = LookupEdition {
        store: book.source.trim().to_ascii_lowercase(),
        marketplace: book.marketplace.trim().to_ascii_lowercase(),
        product_id: book.product_id.clone(),
        availability: Availability::Owned,
        formats,
        metadata: CatalogMetadata {
            title: book.title.clone(),
            subtitle: book.subtitle.clone(),
            authors: book.authors.clone(),
            narrators: book.narrators.clone(),
            series: book.series.clone(),
            series_index: book.series_index.clone(),
            asin: book.asin.clone(),
            isbn: book.isbn.clone(),
            publisher: book.publisher.clone(),
            length_minutes: book.length_minutes,
            content_kind: Some(book.content_kind.clone()),
            language: book.language.clone(),
            published_at: book.published_at.clone(),
        },
    };
    edition.problem().is_none().then_some(edition)
}

/// Answers questions a library sent this branch from this branch's own
/// library.
///
/// Each question is mapped to a work here by identifier
/// (`work_for_question`); the answer lists the books of that work this
/// library holds whose holder may be listed (the owner's always; members'
/// only with `list_members_books` on and when they did not opt out), at
/// most [`MAX_LOOKUP_EDITIONS`]. A question with no such book is answered
/// as missing.
///
/// # Arguments
///
/// * `questions` - The library's questions (already validated).
/// * `list_members_books` - The link's "List my members' books" setting.
///
/// # Returns
///
/// The answers and the missing works, together naming every question.
///
/// # Errors
///
/// Returns the read error.
pub async fn answer_questions(
    store: &LibraryStore,
    questions: &[LookupWork],
    list_members_books: bool,
) -> Result<(Vec<LookupAnswer>, Vec<String>)> {
    let mut answers = Vec::new();
    let mut missing = Vec::new();
    for question in questions.iter().take(MAX_LOOKUP_WORKS) {
        let Some(work_id) = work_for_question(store, question).await? else {
            missing.push(question.work.clone());
            continue;
        };
        let edition_ids: Vec<String> = editions_of_works(store, std::slice::from_ref(&work_id))
            .await?
            .into_keys()
            .collect();
        let listings = read_chunked(store, &edition_ids, |part| {
            store_listings::Entity::find().filter(store_listings::Column::EditionId.is_in(part))
        })
        .await?;
        let keys: BTreeSet<(String, String, String)> = listings
            .iter()
            .map(|l| {
                (
                    l.plugin_key.clone(),
                    l.marketplace.clone(),
                    l.product_id.clone(),
                )
            })
            .collect();
        let products: Vec<String> = keys
            .iter()
            .map(|(_, _, p)| p.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let held: Vec<books::Model> = read_chunked(store, &products, |part| {
            books::Entity::find()
                .filter(books::Column::ProductId.is_in(part))
                .order_by_asc(books::Column::Id)
        })
        .await?
        .into_iter()
        .filter(|b| {
            keys.contains(&(
                b.plugin_key.clone(),
                b.marketplace.to_ascii_lowercase(),
                b.product_id.clone(),
            ))
        })
        .collect();
        let listers = Listers::load(store, &held).await?;
        let mut editions: Vec<LookupEdition> = Vec::new();
        for book in &held {
            if editions.len() >= MAX_LOOKUP_EDITIONS || !listers.may_list(book, list_members_books)
            {
                continue;
            }
            let Some(edition) = book_edition(book) else {
                continue;
            };
            let same = editions.iter().position(|e| {
                e.store == edition.store
                    && e.marketplace == edition.marketplace
                    && e.product_id == edition.product_id
            });
            match same {
                // Two people hold it: keep the copy with more formats.
                Some(at) if editions[at].formats.len() < edition.formats.len() => {
                    editions[at] = edition;
                }
                Some(_) => {}
                None => editions.push(edition),
            }
        }
        if editions.is_empty() {
            missing.push(question.work.clone());
        } else {
            answers.push(LookupAnswer {
                work: question.work.clone(),
                editions,
            });
        }
    }
    Ok((answers, missing))
}

#[cfg(test)]
#[path = "catalog_lookup_tests.rs"]
mod tests;
