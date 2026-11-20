//! Books in other libraries: a branch shows the books of the library it
//! joined, and a clerk those of each library it holds a library card for,
//! merged by work into one list ("window books"). A branch reads them
//! whatever its "Keep copies on this computer" setting says.
//!
//! On the library, [`window_books_page`] answers the `window.books` frame:
//! one page of the books on every store account of this library, as its
//! own Library page lists them for a member, grouped by this library's
//! works, each work with its identifiers. Each edition is marked
//! [`WindowEdition::yours`] when it is on a store account linked to the
//! node's owner (the person). Only a branch this library trusts is
//! answered.
//! [`person_books_page`] answers the same page for a person directly: a
//! clerk signed in with that person's library card reads it over the
//! clerk socket (`GET /api/clerk/v1/connect`).
//!
//! On the reading install (a branch or a clerk):
//!
//! 1. [`record_window_page`] stores each page, one `window_books` row per
//!    edition, with the work of this install's own library its identifiers
//!    reach (by identifier, never by title; none when no single work does).
//! 2. [`finish_window_read`] drops the rows a complete read no longer
//!    named, so a book the library stopped holding goes away.
//!    [`clear_window_books`] drops a library's rows outright (the link
//!    left, or the card signed out).
//! 3. [`window_view`] merges every link's and card's rows into one list: rows
//!    are the same work when they reach the same local work, share a
//!    strong identifier (ASIN with marketplace, OpenLibrary edition or work
//!    id), or are grouped together by the same library.
//!
//! Work ids are local to each library, so a library's work id is only a
//! grouping key inside that library and is never compared across
//! libraries.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bookclerk_node_proto::{
    Availability, CatalogMetadata, LookupEdition, LookupFormat, LookupIdentifier, LookupScheme,
    LookupWork, WindowCopy, WindowCursor, WindowEdition, WindowWork, MAX_FRAME_BYTES,
    MAX_LOOKUP_EDITIONS, MAX_PROBE_BITRATE_KBPS, MAX_WINDOW_WORKS,
};
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::batch::{exec, int, text, text_opt};
use super::catalog_lookup::{
    identifiers_of, in_chunk, is_result_bytes_limit, is_too_wide_to_read, is_trusted_branch,
    listed_titles, read_chunked, read_chunked_all, read_paged, read_paged_by, result_page,
    run_batches, stamp, title_edition, trusted_branches, work_for_question, works_of_editions,
    BRANCH_LISTING_PREFIX, BRANCH_LISTING_TTL, PAGED_FIXED_BINDS,
};
use super::clerk_links::list_clerk_links;
use super::library_links::list_library_links;
use crate::entities::{
    account_links, edition_acquisitions, edition_files, holdings, nodes, portal_identities,
    store_listings, window_book_parts, window_books,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;
use crate::store::holdings::find_holdings;
use crate::store::{AcquireTitle, FileRole, LibraryStore};

/// Most holdings one [`window_books_page`] reads.
pub const WINDOW_BOOKS_READ: u64 = 200;

/// Bytes of works one `window.page` frame carries at most, well under
/// [`MAX_FRAME_BYTES`] so the frame's envelope always fits.
const WINDOW_PAGE_BUDGET: usize = MAX_FRAME_BYTES / 2;

/// The store accounts linked to `user_id`, as `(plugin key, account id)`.
///
/// Both the person's sign-in identities and each identity's account links
/// are read a database result page at a time.
async fn person_accounts(store: &LibraryStore, user_id: i64) -> Result<BTreeSet<(String, String)>> {
    let identities = read_paged(
        store,
        portal_identities::Entity::find().filter(portal_identities::Column::UserId.eq(user_id)),
        None,
    )
    .await?;
    let mut accounts = BTreeSet::new();
    for identity in identities {
        let links = read_paged(
            store,
            account_links::Entity::find().filter(account_links::Column::IdentityId.eq(identity.id)),
            None,
        )
        .await?;
        for link in links {
            accounts.insert((link.plugin_key, link.account_id));
        }
    }
    Ok(accounts)
}

/// The media type an audio file's name implies, for a row no probe
/// described; `None` for an extension that names no audio container.
fn audio_type_of_key(key: &str) -> Option<&'static str> {
    let (_, ext) = key.rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "m4b" | "m4a" | "mp4" => Some("audio/mp4"),
        "mp3" => Some("audio/mpeg"),
        "flac" => Some("audio/flac"),
        _ => None,
    }
}

/// What the library tells a reader about the audio file `row` holds.
///
/// The probe's values when the file was probed; otherwise the media type
/// its name implies, with no bitrate. `None` when neither gives a media
/// type, or a stored value is outside what a page may carry.
fn copy_of_row(row: &edition_files::Model) -> Option<WindowCopy> {
    use sha2::Digest as _;

    let probed = row.probed_at.is_some();
    let content_type = match (&row.content_type, probed) {
        (Some(kind), true) => kind.clone(),
        _ => audio_type_of_key(&row.storage_key)?.to_string(),
    };
    // Any write of the key changes `written_at`, and a new upload its
    // digest: the token names this file and nothing about where it lies.
    let mut hasher = sha2::Sha256::new();
    for part in [
        row.destination.as_str(),
        row.storage_key.as_str(),
        row.written_at.as_str(),
        row.writer.as_str(),
        row.sha256.as_deref().unwrap_or(""),
    ] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    let copy = WindowCopy {
        source_store: row.source_store.clone().filter(|_| probed),
        content_type,
        bitrate_kbps: row
            .bitrate_kbps
            .filter(|_| probed)
            .and_then(|kbps| u32::try_from(kbps).ok())
            .filter(|kbps| (1..=MAX_PROBE_BITRATE_KBPS).contains(kbps)),
        lossless: probed && row.lossless != 0,
        version: digest.chars().take(32).collect(),
    };
    copy.is_well_formed().then_some(copy)
}

/// The copy this library holds of each of `edition_ids`, by edition id:
/// the edition's acquired primary audio file as its row describes it
/// ([`copy_of_row`]).
///
/// An edition that is not acquired, or whose primary file has no row, has
/// no entry. Where the file lies on several destinations, the probed row
/// is used, then the first destination by name, so the answer is stable.
///
/// # Errors
///
/// Returns the read error.
pub(crate) async fn copies_of_editions(
    store: &LibraryStore,
    edition_ids: &[String],
) -> Result<HashMap<String, WindowCopy>> {
    let primary: HashMap<String, String> = read_chunked(store, edition_ids, |part| {
        edition_acquisitions::Entity::find()
            .filter(edition_acquisitions::Column::EditionId.is_in(part))
            .filter(edition_acquisitions::Column::Status.eq(AcquireStatus::Acquired.as_str()))
    })
    .await?
    .into_iter()
    .filter_map(|row| Some((row.edition_id, row.storage_key?)))
    .collect();
    if primary.is_empty() {
        return Ok(HashMap::new());
    }
    let acquired: Vec<String> = primary
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut rows = read_chunked_all(store, &acquired, |part| {
        edition_files::Entity::find()
            .filter(edition_files::Column::OwnerKind.eq("edition"))
            .filter(edition_files::Column::OwnerId.is_in(part))
            .filter(edition_files::Column::Role.eq(FileRole::Audio.as_str()))
    })
    .await?;
    rows.sort_by(|a, b| {
        (a.probed_at.is_none(), &a.destination).cmp(&(b.probed_at.is_none(), &b.destination))
    });
    let mut copies = HashMap::new();
    for row in rows {
        if primary.get(&row.owner_id) != Some(&row.storage_key)
            || copies.contains_key(&row.owner_id)
        {
            continue;
        }
        if let Some(copy) = copy_of_row(&row) {
            copies.insert(row.owner_id, copy);
        }
    }
    Ok(copies)
}

/// True when `a` and `b` are the same store listing: the same store,
/// marketplace and product id.
fn same_edition(a: &LookupEdition, b: &LookupEdition) -> bool {
    a.store == b.store && a.marketplace == b.marketplace && a.product_id == b.product_id
}

/// The page key of a work's `n`th overflow group (`<work>-x<n>`). Work ids
/// are UUIDs (hex digits and dashes) or `book-<id>`, so the suffix never
/// ends a real key and [`grouping_key`] can take it off again.
fn overflow_key(work: &str, n: usize) -> String {
    format!("{work}-x{n}")
}

/// The library's grouping key of page key `key`: the key without an
/// [`overflow_key`] suffix, so a work's overflow editions group with the
/// rest of it.
fn grouping_key(key: &str) -> &str {
    match key.rsplit_once("-x") {
        Some((work, n))
            if !work.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) =>
        {
            work
        }
        _ => key,
    }
}

/// Encoded size of `value` in bytes, plus one for the separator; the whole
/// budget when it cannot be encoded, so it never fits.
fn encoded_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(WINDOW_PAGE_BUDGET, |bytes| bytes.len() + 1)
}

/// The titles of `held` (account holdings, in id order), as acquire
/// reads them.
///
/// A title's listing carries all its store metadata, which together can
/// pass the plugin's `maxResultBytes` even when every field fits a cell
/// (and a value stored before a smaller `maxCellBytes` can pass that).
/// When the read is refused for its width, each holding is read on its
/// own; a title still too wide to read is left out of the window (and
/// logged) instead of failing every page after it.
///
/// # Errors
///
/// Returns any read error other than a title too wide to read.
pub(crate) async fn titles_within_bytes(
    store: &LibraryStore,
    held: Vec<holdings::Model>,
) -> Result<Vec<AcquireTitle>> {
    match store.acquire_titles_of(held.clone()).await {
        Ok(titles) => return Ok(titles),
        Err(err) if is_too_wide_to_read(&err) => {}
        Err(err) => return Err(err),
    }
    let mut titles = Vec::with_capacity(held.len());
    for holding in held {
        let id = holding.id.clone();
        match store.acquire_titles_of(vec![holding]).await {
            Ok(one) => titles.extend(one),
            Err(err) if is_too_wide_to_read(&err) => {
                tracing::warn!(
                    holding_id = %id,
                    error = %err,
                    "title left out of the window: its listing is wider than one database reply"
                );
            }
            Err(err) => return Err(err),
        }
    }
    Ok(titles)
}

/// One page of the books this library holds for branch `node_id`'s owner,
/// for the `window.books` frame.
///
/// The library's titles (every account holding, each marked the person's
/// when it is on one of their accounts) are read in holding id order from
/// cursor `after` (at most [`WINDOW_BOOKS_READ`] holdings, and
/// no more than one database result page); the next cursor is the last
/// holding id the page covered. Each title is grouped under its work here
/// (its key is the work id, or `edition-<edition id>` when the edition has
/// no work), with the work's identifiers; a work
/// lists at most [`MAX_LOOKUP_EDITIONS`] editions (where two accounts hold
/// the same edition, one entry with every format either copy has, the
/// person's when either account is).
/// The page stops early once it would pass the frame budget or
/// [`MAX_WINDOW_WORKS`] works; the books after that come on the next page.
///
/// # Arguments
///
/// * `node_id` - The asking node; its owner is the person.
/// * `after` - The cursor of the previous page (empty for the first).
///
/// # Returns
///
/// `None` when `node_id` is not a branch this library trusts; otherwise
/// the page's works and the cursor of the next page (`None` on the last).
///
/// # Errors
///
/// Returns the read error.
pub async fn window_books_page(
    store: &LibraryStore,
    node_id: &str,
    after: &WindowCursor,
) -> Result<Option<(Vec<WindowWork>, Option<WindowCursor>)>> {
    let page = u64::try_from(result_page(store)).unwrap_or(1);
    window_books_page_by(store, node_id, after, page).await
}

/// [`window_books_page`] reading at most `page` books per database read
/// (at least 1; never more than [`WINDOW_BOOKS_READ`]).
///
/// # Errors
///
/// Returns the read error.
async fn window_books_page_by(
    store: &LibraryStore,
    node_id: &str,
    after: &WindowCursor,
    page: u64,
) -> Result<Option<(Vec<WindowWork>, Option<WindowCursor>)>> {
    let Some(node) = nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if !is_trusted_branch(store, &node).await? {
        return Ok(None);
    }
    let Some(person) = node.owner_user_id else {
        return Ok(None);
    };
    person_books_page_by(store, person, after, page, BranchBooks::Leave)
        .await
        .map(Some)
}

/// One page of this library's books for a person, as a clerk signed in
/// with that person's library card reads them over the client
/// socket (`GET /api/clerk/v1/connect`): the same works, grouping and
/// limits as [`window_books_page`], for the person the card belongs to
/// rather than a branch node's owner.
///
/// After the books on its store accounts, a clerk's pages go on with the
/// books only the library's branches hold ([`branch_books_page`]): each
/// edition [`Availability::Reachable`], for the person to ask the library
/// for with its member request route (`POST /api/branch-requests`).
///
/// # Arguments
///
/// * `user_id` - The person: the card's user on this library.
/// * `after` - The cursor of the previous page (empty for the first).
///
/// # Returns
///
/// The page's works and the cursor of the next page (`None` on the last).
///
/// # Errors
///
/// Returns the read error.
pub async fn person_books_page(
    store: &LibraryStore,
    user_id: i64,
    after: &WindowCursor,
) -> Result<(Vec<WindowWork>, Option<WindowCursor>)> {
    let page = u64::try_from(result_page(store)).unwrap_or(1);
    person_books_page_by(store, user_id, after, page, BranchBooks::Include).await
}

/// Whether a person's pages go on with the books the library's branches
/// list ([`branch_books_page`]) after its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchBooks {
    /// A clerk's pages: the person may ask the library for them.
    Include,
    /// A branch's pages: only the library's own books. A branch answers
    /// for itself and is never told about the library's other branches.
    Leave,
}

/// The cursor that starts the branch part of a clerk's pages. It sorts
/// after every holding id (lower-case UUIDs), so paging still moves
/// forward.
const BRANCH_PHASE: &str = "zz-branch";

/// Where in the branch part of a clerk's pages `after` points: `None` for
/// a cursor in the library's own books, `Some(None)` for the start of the
/// branch part, and `Some(Some(edition id))` for after that edition.
fn branch_phase_after(after: &WindowCursor) -> Option<Option<&str>> {
    let text = after.as_str();
    if text == BRANCH_PHASE {
        return Some(None);
    }
    text.strip_prefix(BRANCH_PHASE)
        .and_then(|rest| rest.strip_prefix('-'))
        .filter(|edition| !edition.is_empty())
        .map(Some)
}

/// The cursor after edition `edition_id` in the branch part of a clerk's
/// pages; `None` when the id cannot make a cursor.
fn branch_cursor(edition_id: &str) -> Option<WindowCursor> {
    WindowCursor::parse(&format!("{BRANCH_PHASE}-{edition_id}"))
}

/// One `edition_id` read alone.
#[derive(Debug, sea_orm::FromQueryResult)]
struct EditionIdRow {
    /// `store_listings.edition_id`.
    edition_id: String,
}

/// One `store_listings.id` read alone.
#[derive(Debug, sea_orm::FromQueryResult)]
struct ListingIdRow {
    /// `store_listings.id`.
    id: String,
}

/// True when any branch listing is stored, current or not: whether a
/// clerk's pages need a branch part at all.
///
/// # Errors
///
/// Returns the read error.
async fn any_branch_listing(store: &LibraryStore) -> Result<bool> {
    Ok(store_listings::Entity::find()
        .select_only()
        .column(store_listings::Column::Id)
        .filter(store_listings::Column::PluginKey.starts_with(BRANCH_LISTING_PREFIX))
        .limit(1)
        .into_model::<ListingIdRow>()
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some_and(|row| !row.id.is_empty()))
}

/// The editions of `edition_ids` this library holds on a store account:
/// those with a listing (of any store) that an account holding names.
///
/// # Errors
///
/// Returns the read error.
async fn editions_held_on_accounts(
    store: &LibraryStore,
    edition_ids: &[String],
) -> Result<BTreeSet<String>> {
    let listings = read_chunked_all(store, edition_ids, |part| {
        store_listings::Entity::find().filter(store_listings::Column::EditionId.is_in(part))
    })
    .await?;
    let edition_of: HashMap<String, String> = listings
        .into_iter()
        .filter(|l| !l.plugin_key.starts_with(BRANCH_LISTING_PREFIX))
        .map(|l| (l.id, l.edition_id))
        .collect();
    let listing_ids: Vec<String> = edition_of.keys().cloned().collect();
    Ok(read_chunked_all(store, &listing_ids, |part| {
        holdings::Entity::find()
            .filter(holdings::Column::ListingId.is_in(part))
            .filter(holdings::Column::HolderKind.eq("account"))
    })
    .await?
    .into_iter()
    .filter_map(|h| edition_of.get(&h.listing_id).cloned())
    .collect())
}

/// The edition a branch listing names, as a clerk's page carries it:
/// [`Availability::Reachable`], with no formats (the library holds no
/// copy) and the listing's own metadata. `None` when the listing does not
/// make a well-formed edition.
fn branch_edition(row: &store_listings::Model) -> Option<LookupEdition> {
    let (store_id, product_id) = row.product_id.split_once(':')?;
    let edition = LookupEdition {
        store: store_id.trim().to_ascii_lowercase(),
        marketplace: row.marketplace.trim().to_ascii_lowercase(),
        product_id: product_id.to_string(),
        availability: Availability::Reachable,
        formats: Vec::new(),
        metadata: CatalogMetadata {
            title: row.title.clone(),
            subtitle: row.subtitle.clone(),
            authors: row.authors.clone(),
            narrators: row.narrators.clone(),
            series: row.series.clone(),
            series_index: row.series_index.clone(),
            asin: None,
            isbn: None,
            publisher: row.publisher.clone(),
            length_minutes: row.length_minutes,
            content_kind: None,
            language: row.language.clone(),
            published_at: row.published_at.clone(),
        },
    };
    edition.problem().is_none().then_some(edition)
}

/// One page of the branch part of a clerk's pages: the editions a branch
/// this library trusts listed within [`BRANCH_LISTING_TTL`] (the same
/// listings Discover and search show as available through a branch, and
/// that `POST /api/branch-requests` asks for), that no store account here
/// holds.
///
/// Editions are read in edition id order after `after` (at most
/// [`WINDOW_BOOKS_READ`] and no more than `page` per read), grouped under
/// their work with the work's identifiers like the library's own books,
/// each [`Availability::Reachable`] and never the person's. A page names
/// no branch: the library picks one when the person asks. The next cursor
/// is the last edition the page covered.
///
/// # Errors
///
/// Returns the read error.
async fn branch_books_page(
    store: &LibraryStore,
    after: Option<&str>,
    page: u64,
) -> Result<(Vec<WindowWork>, Option<WindowCursor>)> {
    let limit = WINDOW_BOOKS_READ.min(page.max(1));
    let mut select = store_listings::Entity::find()
        .select_only()
        .column(store_listings::Column::EditionId)
        .distinct()
        .filter(store_listings::Column::PluginKey.starts_with(BRANCH_LISTING_PREFIX))
        .order_by_asc(store_listings::Column::EditionId)
        .limit(limit);
    if let Some(after) = after {
        select = select.filter(store_listings::Column::EditionId.gt(after));
    }
    let edition_ids: Vec<String> = select
        .into_model::<EditionIdRow>()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|row| row.edition_id)
        .collect();
    let full = u64::try_from(edition_ids.len()).unwrap_or(u64::MAX) >= limit;
    let trusted: BTreeSet<String> = trusted_branches(store).await?.into_iter().collect();
    let since = Utc::now() - BRANCH_LISTING_TTL;
    let mut current: BTreeMap<String, Vec<store_listings::Model>> = BTreeMap::new();
    for row in read_chunked_all(store, &edition_ids, |part| {
        store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.is_in(part))
            .filter(store_listings::Column::PluginKey.starts_with(BRANCH_LISTING_PREFIX))
    })
    .await?
    {
        let node = row.plugin_key.strip_prefix(BRANCH_LISTING_PREFIX);
        let fresh = chrono::DateTime::parse_from_rfc3339(&row.observed_at)
            .is_ok_and(|seen| seen.with_timezone(&Utc) >= since);
        if fresh && node.is_some_and(|node| trusted.contains(node)) {
            current.entry(row.edition_id.clone()).or_default().push(row);
        }
    }
    let listed: Vec<String> = current.keys().cloned().collect();
    let held = editions_held_on_accounts(store, &listed).await?;
    let work_of_edition = works_of_editions(store, &listed).await?;
    let work_ids: Vec<String> = work_of_edition
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let identifiers = identifiers_of(store, &work_ids).await?;
    let mut builder = PageBuilder::default();
    let mut last: Option<&String> = None;
    let mut stopped = false;
    'editions: for edition_id in &edition_ids {
        // Listed but held here (the library's own pages carry it), or with
        // no work to ask for: nothing to request.
        let work = work_of_edition
            .get(edition_id)
            .filter(|_| !held.contains(edition_id));
        if let (Some(work), Some(rows)) = (work, current.get(edition_id)) {
            for edition in rows.iter().filter_map(branch_edition) {
                let edition = WindowEdition {
                    edition,
                    yours: false,
                    copy: None,
                };
                if !builder.push(work, identifiers.get(work), edition) {
                    stopped = true;
                    break 'editions;
                }
            }
        }
        last = Some(edition_id);
    }
    let next = (stopped || full)
        .then_some(last)
        .flatten()
        .and_then(|edition| branch_cursor(edition));
    Ok((builder.works, next))
}

/// The library's account holdings after cursor `after`, at most `limit`, in
/// holding id order: every book on any store account here, whoever's
/// account it is.
///
/// # Errors
///
/// Returns the read error.
async fn held_after(
    store: &LibraryStore,
    after: &WindowCursor,
    limit: u64,
) -> Result<Vec<holdings::Model>> {
    let mut select = find_holdings()
        .filter(holdings::Column::HolderKind.eq("account"))
        .order_by_asc(holdings::Column::Id)
        .limit(limit);
    if !after.is_start() {
        select = select.filter(holdings::Column::Id.gt(after.as_str()));
    }
    select.all(store.db()).await.map_err(LibraryError::Orm)
}

/// [`person_books_page`] reading at most `page` books per database read
/// (at least 1; never more than [`WINDOW_BOOKS_READ`]).
///
/// # Errors
///
/// Returns the read error.
async fn person_books_page_by(
    store: &LibraryStore,
    person: i64,
    after: &WindowCursor,
    page: u64,
    branch_books: BranchBooks,
) -> Result<(Vec<WindowWork>, Option<WindowCursor>)> {
    if branch_books == BranchBooks::Include {
        if let Some(edition) = branch_phase_after(after) {
            return branch_books_page(store, edition, page).await;
        }
    }
    // The person sees every book on the library's store accounts, as the
    // library's own Library page lists them; the ones on an account of
    // their own are marked theirs.
    let accounts = person_accounts(store, person).await?;
    let mut limit = WINDOW_BOOKS_READ.min(page.max(1));
    // Holding rows are narrow, but a plugin may advertise a small
    // `maxResultBytes`: when a read is refused for its bytes, the page is
    // read again with half as many rows per read.
    let read = loop {
        match held_after(store, after, limit).await {
            Ok(found) => break found,
            Err(err) if limit > 1 && is_result_bytes_limit(&err) => limit /= 2,
            Err(err) => return Err(err),
        }
    };
    let full = u64::try_from(read.len()).unwrap_or(u64::MAX) >= limit;
    let read_last: Option<String> = read.last().map(|h| h.id.clone());
    let yours: BTreeSet<String> = read
        .iter()
        .filter(|h| match (&h.plugin_key, &h.account_id) {
            (Some(plugin), Some(account)) => accounts.contains(&(plugin.clone(), account.clone())),
            _ => false,
        })
        .map(|h| h.id.clone())
        .collect();
    let held = read;
    let titles = listed_titles(store, titles_within_bytes(store, held).await?).await?;
    let edition_ids: Vec<String> = titles
        .iter()
        .map(|t| t.title.edition_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let work_of_edition = works_of_editions(store, &edition_ids).await?;
    let copies = copies_of_editions(store, &edition_ids).await?;
    let work_ids: Vec<String> = work_of_edition
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let identifiers = identifiers_of(store, &work_ids).await?;

    let mut builder = PageBuilder::default();
    let mut last: Option<String> = None;
    let mut stopped = false;
    for listed in &titles {
        let title = &listed.title;
        let Some(edition) = title_edition(listed) else {
            last = Some(title.holding_id.clone());
            continue;
        };
        let edition = WindowEdition {
            edition,
            yours: yours.contains(&title.holding_id),
            copy: copies.get(&title.edition_id).cloned(),
        };
        let work = work_of_edition
            .get(&title.edition_id)
            .cloned()
            .unwrap_or_else(|| format!("edition-{}", title.edition_id));
        if !builder.push(&work, identifiers.get(&work), edition) {
            stopped = true;
            break;
        }
        last = Some(title.holding_id.clone());
    }
    if !stopped {
        // Holdings of other plugins' accounts, and titles too wide to
        // read, were read past too.
        last = read_last;
    }
    let next = (stopped || full)
        .then_some(last)
        .flatten()
        .and_then(|id| WindowCursor::parse(&id))
        .filter(|next| next > after);
    // The library's own books are done: a clerk's pages go on with those
    // only its branches hold, when any branch listed anything.
    let next = match next {
        None if branch_books == BranchBooks::Include && any_branch_listing(store).await? => {
            WindowCursor::parse(BRANCH_PHASE).filter(|next| next > after)
        }
        next => next,
    };
    Ok((builder.works, next))
}

/// A page of works being filled: editions grouped under their work's key,
/// within the frame budget and [`MAX_WINDOW_WORKS`].
#[derive(Default)]
struct PageBuilder {
    /// The page's works, in the order they were first named.
    works: Vec<WindowWork>,
    /// Each page key's place in `works`.
    index: HashMap<String, usize>,
    /// Encoded bytes the page carries so far.
    used: usize,
}

impl PageBuilder {
    /// Adds `edition` under `work` (with the work's `identifiers`).
    ///
    /// A work already listing the most editions a frame allows goes on
    /// under another key of this page (`<work>-x<n>`) with the same
    /// identifiers, so the reader still merges them. An edition the work
    /// already lists is joined into it: the library has every format
    /// either has, and it is the person's when either is.
    ///
    /// # Returns
    ///
    /// `false`, adding nothing, when the edition would pass the frame
    /// budget or need a work past [`MAX_WINDOW_WORKS`]; the page is then
    /// full. The first edition of a page always fits.
    fn push(
        &mut self,
        work: &str,
        identifiers: Option<&Vec<LookupIdentifier>>,
        edition: WindowEdition,
    ) -> bool {
        let mut key = work.to_string();
        let mut overflow = 0;
        while let Some(&at) = self.index.get(&key) {
            let editions = &self.works[at].editions;
            let listed = editions
                .iter()
                .any(|e| same_edition(&e.edition, &edition.edition));
            if listed || editions.len() < MAX_LOOKUP_EDITIONS {
                break;
            }
            overflow += 1;
            key = overflow_key(work, overflow);
        }
        let mut size = encoded_len(&edition);
        let at = self.index.get(&key).copied();
        if at.is_none() {
            size += encoded_len(&key)
                + identifiers.map_or(0, |ids| ids.iter().map(encoded_len).sum::<usize>());
        }
        if !self.works.is_empty()
            && (self.used + size > WINDOW_PAGE_BUDGET
                || (at.is_none() && self.works.len() >= MAX_WINDOW_WORKS))
        {
            return false;
        }
        self.used += size;
        let at = match at {
            Some(at) => at,
            None => {
                self.works.push(WindowWork {
                    work: key.clone(),
                    identifiers: identifiers.cloned().unwrap_or_default(),
                    editions: Vec::new(),
                });
                self.index.insert(key, self.works.len() - 1);
                self.works.len() - 1
            }
        };
        let editions = &mut self.works[at].editions;
        match editions
            .iter()
            .position(|e| same_edition(&e.edition, &edition.edition))
        {
            Some(i) => {
                let listed = &mut editions[i];
                listed.yours |= edition.yours;
                let held = &mut listed.edition.formats;
                held.extend(edition.edition.formats);
                held.sort_unstable();
                held.dedup();
            }
            None => editions.push(edition),
        }
        true
    }
}

/// A stamp for one complete read of a library's window books: pass it to
/// every [`record_window_page`] of the read and then to
/// [`finish_window_read`].
#[must_use]
pub fn window_read_stamp() -> String {
    stamp(Utc::now())
}

/// Stores one `window.page` from library `parent_library_id`, read under
/// `seen_at` ([`window_read_stamp`]).
///
/// Each edition becomes (or refreshes) one `window_books` row. Each work is
/// placed on a work of this install's library when its strong identifiers
/// reach exactly one; none otherwise. An ISBN never places a work: one can
/// sit on editions of different works, and the local work joins rows from
/// every library in the view.
///
/// # Arguments
///
/// * `works` - The page, already validated against the frame rules.
///
/// # Returns
///
/// How many editions were stored.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn record_window_page(
    store: &LibraryStore,
    parent_library_id: &str,
    seen_at: &str,
    works: &[WindowWork],
) -> Result<usize> {
    let earlier = earlier_in_read(store, parent_library_id, seen_at, works).await?;
    let width = super::artifacts::chunk_bytes(store.db_capabilities());
    let mut groups = Vec::new();
    let mut stored = 0;
    for work in works {
        let strong: Vec<LookupIdentifier> = work
            .identifiers
            .iter()
            .filter(|id| id.scheme != LookupScheme::Isbn)
            .cloned()
            .collect();
        let local = if strong.is_empty() {
            None
        } else {
            work_for_question(
                store,
                &LookupWork {
                    work: work.work.clone(),
                    identifiers: strong,
                },
            )
            .await?
        };
        for listed in &work.editions {
            let edition = &listed.edition;
            let key = (
                edition.store.clone(),
                edition.marketplace.clone(),
                edition.product_id.clone(),
            );
            let merged;
            let mut yours = listed.yours;
            // Every page of a read states the same copy of an edition;
            // one that states none keeps what an earlier page said.
            let copy = listed
                .copy
                .clone()
                .or_else(|| earlier.get(&key).and_then(|before| before.copy.clone()));
            let edition = match earlier.get(&key) {
                Some(before) => {
                    let mut richer = edition.clone();
                    richer.formats = before
                        .formats
                        .iter()
                        .chain(&edition.formats)
                        .copied()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    yours |= before.yours;
                    // Held on a store account on any page of the read wins
                    // over only listed by a branch.
                    if before.availability == Availability::Owned {
                        richer.availability = Availability::Owned;
                    }
                    merged = richer;
                    &merged
                }
                None => edition,
            };
            let json = serde_json::to_string(&StoredBook {
                identifiers: work.identifiers.clone(),
                edition: edition.clone(),
                yours,
                copy,
            })
            .map_err(|err| LibraryError::Invalid(err.to_string()))?;
            // The pieces go in first under a new version, then the row
            // names that version: statements commit in order, so a row
            // never names a version whose pieces are not all in, and a
            // write cut short leaves the row on its previous version.
            let version = uuid::Uuid::new_v4().simple().to_string();
            for (seq, body) in (0_i64..).zip(cut(&json, width)) {
                groups.push(vec![exec(
                    "INSERT INTO window_book_parts (parent_library_id, store, marketplace, \
                     product_id, version, seq, body) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    vec![
                        text(parent_library_id),
                        text(&edition.store),
                        text(&edition.marketplace),
                        text(&edition.product_id),
                        text(&version),
                        int(seq),
                        text(body),
                    ],
                )]);
            }
            groups.push(vec![
                exec(
                    "UPDATE window_books SET work_ref = ?, local_work_id = ?, \
                     parts_version = ?, seen_at = ? WHERE parent_library_id = ? AND store = ? \
                     AND marketplace = ? AND product_id = ?",
                    vec![
                        text(grouping_key(&work.work)),
                        text_opt(local.as_deref()),
                        text(&version),
                        text(seen_at),
                        text(parent_library_id),
                        text(&edition.store),
                        text(&edition.marketplace),
                        text(&edition.product_id),
                    ],
                ),
                exec(
                    "INSERT OR IGNORE INTO window_books (parent_library_id, store, \
                     marketplace, product_id, work_ref, local_work_id, parts_version, \
                     seen_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    vec![
                        text(parent_library_id),
                        text(&edition.store),
                        text(&edition.marketplace),
                        text(&edition.product_id),
                        text(grouping_key(&work.work)),
                        text_opt(local.as_deref()),
                        text(&version),
                        text(seen_at),
                    ],
                ),
            ]);
            stored += 1;
        }
    }
    run_groups(store, groups).await?;
    Ok(stored)
}

/// Runs `groups` in order as atomic batches sized to the database plugin,
/// each group whole within one batch.
///
/// # Errors
///
/// Returns the write error, or an error when one group cannot fit a batch.
async fn run_groups(
    store: &LibraryStore,
    groups: Vec<Vec<bookclerk_plugin_abi::TypedDbStatement>>,
) -> Result<()> {
    let mut batches =
        crate::store::write_batches::WriteBatches::new(store.db_capabilities(), "window book");
    let mut full = Vec::new();
    for group in groups {
        if let Some(batch) = batches.push_group(group)? {
            full.push(batch);
        }
    }
    full.extend(batches.finish());
    for batch in full {
        let operation_id = format!("window-books-{}", uuid::Uuid::new_v4());
        store
            .execute_host_batch(super::batch::request(&operation_id, batch))
            .await?;
    }
    Ok(())
}

/// Room a piece's row takes in a reply beside its body: its key columns
/// and the reply's framing.
const PART_ROW_OVERHEAD: usize = 512;

/// Every piece `select` reads, in pages small enough that a page of
/// full-width pieces stays within the plugin's `maxResultBytes` as well
/// as its `maxResultRows`. The estimate is only a starting size: a page
/// the plugin still refuses for its bytes, inline or streamed (longer keys than
/// [`PART_ROW_OVERHEAD`] allows for) is read again at half the rows.
///
/// # Errors
///
/// Returns the read error.
async fn read_parts(
    store: &LibraryStore,
    select: sea_orm::Select<window_book_parts::Entity>,
) -> Result<Vec<window_book_parts::Model>> {
    let caps = store.db_capabilities();
    let width = super::artifacts::chunk_bytes(caps).saturating_add(PART_ROW_OVERHEAD);
    let by_bytes = match caps.max_result_bytes {
        0 => usize::MAX,
        bytes => usize::try_from(bytes).unwrap_or(usize::MAX) / width,
    };
    read_paged_by(store, select, None, result_page(store).min(by_bytes)).await
}

/// Drops library `parent_library_id`'s pieces no row names (left by a
/// newer write, a dropped row, or a write cut short).
///
/// # Errors
///
/// Returns the write error.
async fn drop_unnamed_parts(store: &LibraryStore, parent_library_id: &str) -> Result<()> {
    run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_book_parts WHERE parent_library_id = ? AND version NOT IN \
             (SELECT parts_version FROM window_books WHERE parent_library_id = ?)",
            vec![text(parent_library_id), text(parent_library_id)],
        )],
    )
    .await?;
    Ok(())
}

/// What one read already stored of an edition: its formats, whether it
/// was the person's, and the library's copy.
struct Earlier {
    /// The formats stored so far in this read.
    formats: Vec<LookupFormat>,
    /// Whether an earlier copy in this read was the person's.
    yours: bool,
    /// The library's copy as an earlier page of this read stated it.
    copy: Option<WindowCopy>,
    /// Whether an earlier page held it on a store account there or only
    /// a branch listed it.
    availability: Availability,
}

/// What this same read (stamped `seen_at`) already stored for the editions
/// of `works`, by `(store, marketplace, product id)`.
///
/// Two accounts can hold one edition with their books on different pages;
/// the second copy then adds its formats to the first instead of replacing
/// them, and the edition stays the person's when either copy is. A later
/// complete read starts from its own stamp, so a format no longer held, or
/// an account no longer the person's, still goes.
async fn earlier_in_read(
    store: &LibraryStore,
    parent_library_id: &str,
    seen_at: &str,
    works: &[WindowWork],
) -> Result<HashMap<(String, String, String), Earlier>> {
    let wanted: BTreeSet<(String, String, String)> = works
        .iter()
        .flat_map(|work| &work.editions)
        .map(|e| &e.edition)
        .map(|e| (e.store.clone(), e.marketplace.clone(), e.product_id.clone()))
        .collect();
    let products: Vec<String> = wanted
        .iter()
        .map(|(_, _, product)| product.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let owner = parent_library_id.to_string();
    let stamp_now = seen_at.to_string();
    let rows = read_chunked_all(store, &products, move |part| {
        window_books::Entity::find()
            .filter(window_books::Column::ParentLibraryId.eq(owner.clone()))
            .filter(window_books::Column::SeenAt.eq(stamp_now.clone()))
            .filter(window_books::Column::ProductId.is_in(part))
    })
    .await?;
    let named: BTreeSet<PartsKey> = rows
        .into_iter()
        .map(|row| {
            (
                (row.store, row.marketplace, row.product_id),
                row.parts_version,
            )
        })
        .filter(|(key, _)| wanted.contains(key))
        .collect();
    if named.is_empty() {
        return Ok(HashMap::new());
    }
    let versions: Vec<String> = named.iter().map(|(_, version)| version.clone()).collect();
    let mut parts = Vec::new();
    // Room for the owner filter and the page's limit and offset beside the
    // versions.
    for chunk in versions.chunks(in_chunk(store, PAGED_FIXED_BINDS)) {
        parts.extend(
            read_parts(
                store,
                window_book_parts::Entity::find()
                    .filter(window_book_parts::Column::ParentLibraryId.eq(parent_library_id))
                    .filter(window_book_parts::Column::Version.is_in(chunk.to_vec())),
            )
            .await?,
        );
    }
    Ok(join_parts(parts)
        .into_iter()
        .filter(|(key, _)| named.contains(key))
        .map(|((key, _), book)| {
            (
                key,
                Earlier {
                    availability: book.edition.availability,
                    formats: book.edition.formats,
                    yours: book.yours,
                    copy: book.copy,
                },
            )
        })
        .collect())
}

/// Ends a complete read of library `parent_library_id`'s window books: drops
/// its rows that read (stamped `seen_at`) did not name. Rows are compared
/// by stamp equality, not order, so a clock moved back between reads still
/// drops what the new read left out.
///
/// # Returns
///
/// How many rows were dropped.
///
/// # Errors
///
/// Returns the write error.
pub async fn finish_window_read(
    store: &LibraryStore,
    parent_library_id: &str,
    seen_at: &str,
) -> Result<u64> {
    let dropped = run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_books WHERE parent_library_id = ? AND seen_at <> ?",
            vec![text(parent_library_id), text(seen_at)],
        )],
    )
    .await?;
    drop_unnamed_parts(store, parent_library_id).await?;
    Ok(dropped)
}

/// Drops the rows a read cut short by a settings change wrote (stamped
/// `seen_at`), so a read that began before downloading was turned on
/// cannot put back the list the change cleared.
///
/// # Returns
///
/// How many rows were dropped.
///
/// # Errors
///
/// Returns the write error.
pub async fn discard_window_read(
    store: &LibraryStore,
    parent_library_id: &str,
    seen_at: &str,
) -> Result<u64> {
    let dropped = run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_books WHERE parent_library_id = ? AND seen_at = ?",
            vec![text(parent_library_id), text(seen_at)],
        )],
    )
    .await?;
    drop_unnamed_parts(store, parent_library_id).await?;
    Ok(dropped)
}

/// Drops every window book of library `parent_library_id` (its link
/// downloads again, or was left).
///
/// # Returns
///
/// How many rows were dropped.
///
/// # Errors
///
/// Returns the write error.
pub async fn clear_window_books(store: &LibraryStore, parent_library_id: &str) -> Result<u64> {
    // Most calls find nothing: read first so they write nothing.
    let any = window_books::Entity::find()
        .filter(window_books::Column::ParentLibraryId.eq(parent_library_id))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let any_part = window_book_parts::Entity::find()
        .filter(window_book_parts::Column::ParentLibraryId.eq(parent_library_id))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if any.is_none() && any_part.is_none() {
        return Ok(0);
    }
    let dropped = run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_books WHERE parent_library_id = ?",
            vec![text(parent_library_id)],
        )],
    )
    .await?;
    run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_book_parts WHERE parent_library_id = ?",
            vec![text(parent_library_id)],
        )],
    )
    .await?;
    Ok(dropped)
}

/// A window book's edition and identifiers as `window_book_parts` holds
/// them, joined.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredBook {
    /// The work's identifiers in the library.
    identifiers: Vec<LookupIdentifier>,
    /// The edition as the library sent it (formats joined within a read).
    edition: LookupEdition,
    /// Whether the person holds it through an account of their own there.
    /// Rows stored before the library said so read as not theirs.
    #[serde(default)]
    yours: bool,
    /// The copy the library holds of it, when it said so. Rows stored
    /// before the library stated copies have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy: Option<WindowCopy>,
}

/// A window book's `(store, marketplace, product id)`.
type BookKey = (String, String, String);

/// One written version of a window book's pieces: its key and version.
type PartsKey = (BookKey, String);

/// Cuts `json` into pieces of at most `width` bytes (never below 4, the
/// widest character), each ending on a character boundary.
fn cut(json: &str, width: usize) -> Vec<&str> {
    let width = width.max(4);
    let mut pieces = Vec::new();
    let mut rest = json;
    while !rest.is_empty() {
        let mut end = width.min(rest.len());
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, tail) = rest.split_at(end);
        pieces.push(piece);
        rest = tail;
    }
    pieces
}

/// Joins `parts` into stored books by key and version. A version whose
/// pieces are not a gapless run from 0, or do not parse, is left out.
fn join_parts(mut parts: Vec<window_book_parts::Model>) -> HashMap<PartsKey, StoredBook> {
    parts.sort_by(|a, b| {
        (&a.store, &a.marketplace, &a.product_id, &a.version, a.seq).cmp(&(
            &b.store,
            &b.marketplace,
            &b.product_id,
            &b.version,
            b.seq,
        ))
    });
    // Each book's text so far and the next piece expected; `None` once a
    // piece is out of place (a gap, or a run not starting at 0).
    let mut texts: BTreeMap<PartsKey, Option<(String, i64)>> = BTreeMap::new();
    for part in parts {
        let key = (
            (part.store, part.marketplace, part.product_id),
            part.version,
        );
        let entry = texts.entry(key).or_insert_with(|| Some((String::new(), 0)));
        *entry = entry.take().and_then(|(mut text, next)| {
            (part.seq == next).then(|| {
                text.push_str(&part.body);
                (text, next + 1)
            })
        });
    }
    texts
        .into_iter()
        .filter_map(|(key, text)| {
            let (text, _) = text?;
            let book = serde_json::from_str::<StoredBook>(&text).ok()?;
            Some((key, book))
        })
        .collect()
}

/// One edition of a [`WindowEntry`], and the library that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntryEdition {
    /// The library (parent library id) that holds it for the person.
    pub parent_library_id: String,
    /// The library's origin, for display.
    pub owner_origin: String,
    /// The edition as that library sent it.
    pub edition: LookupEdition,
    /// Whether the person holds it there through a store account of their
    /// own.
    pub yours: bool,
    /// The copy of its audio that library holds, when it stated one.
    pub copy: Option<WindowCopy>,
    /// That library's own key for the edition's work (its work id, or
    /// `edition-<id>` for an edition with no work there): what to ask the
    /// library for when only one of its branches lists the edition.
    pub work_ref: String,
}

/// One work in the window's merged view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    /// A key for the entry that stays the same while its rows do: the
    /// local work id when it has one, else its first edition's library,
    /// store, marketplace and product id.
    pub key: String,
    /// The work of this install's library the entry was placed on, if any.
    pub work_id: Option<String>,
    /// Every edition held for the person, over every library, in library
    /// then store order.
    pub editions: Vec<WindowEntryEdition>,
}

impl WindowEntry {
    /// The entry's title: its first edition's.
    #[must_use]
    pub fn title(&self) -> &str {
        self.editions
            .first()
            .map_or("", |e| e.edition.metadata.title.as_str())
    }

    /// The formats any library has, deduplicated.
    #[must_use]
    pub fn formats(&self) -> Vec<LookupFormat> {
        let mut formats: Vec<LookupFormat> = self
            .editions
            .iter()
            .flat_map(|e| e.edition.formats.iter().copied())
            .collect();
        formats.sort();
        formats.dedup();
        formats
    }
}

/// Union-find over row indices.
struct Groups {
    /// Each index's parent; a root is its own parent.
    parent: Vec<usize>,
}

impl Groups {
    /// `n` singleton groups.
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    /// The root of `i`'s group.
    fn root(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    /// Joins `a`'s and `b`'s groups.
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.root(a), self.root(b));
        if a != b {
            self.parent[a.max(b)] = a.min(b);
        }
    }
}

/// The keys a row is merged on: its local work, its library's grouping
/// key, and each identifier.
fn merge_keys(row: &window_books::Model, identifiers: &[LookupIdentifier]) -> Vec<String> {
    let mut keys = vec![format!(
        "ref\u{1f}{}\u{1f}{}",
        row.parent_library_id, row.work_ref
    )];
    if let Some(work) = &row.local_work_id {
        keys.push(format!("work\u{1f}{work}"));
    }
    // ISBNs are weak (one can sit on several editions, even of different
    // works), so only strong identifiers join rows across libraries.
    for id in identifiers
        .iter()
        .filter(|id| id.scheme != LookupScheme::Isbn)
    {
        keys.push(format!(
            "id\u{1f}{}\u{1f}{}\u{1f}{}",
            id.scheme.as_str(),
            id.value,
            id.marketplace
        ));
    }
    keys
}

/// The merged view of every library this install is a branch of or holds
/// a library card for: their books, one entry per work, sorted by title.
///
/// Every active link counts, and every active library card ([`crate::control_plane::list_clerk_links`]). Rows
/// are the same work when they reach the same work of this library, share
/// a strong identifier (not an ISBN), or carry the same grouping key from
/// one library. A row whose stored edition is missing or no longer parses
/// is skipped.
///
/// # Errors
///
/// Returns the read error.
pub async fn window_view(store: &LibraryStore) -> Result<Vec<WindowEntry>> {
    let mut windows: BTreeMap<String, String> = list_library_links(store)
        .await?
        .into_iter()
        .filter(|link| link.is_active())
        .map(|link| (link.parent_library_id, link.owner_origin))
        .collect();
    // Libraries this install holds a library card for (as a clerk) read
    // into the same rows. A clerk link is refused while a library link to
    // that library is active, so the two never share a library; should
    // both be found, the branch link's origin is kept.
    for link in list_clerk_links(store).await? {
        if link.is_active() {
            windows.entry(link.library_id).or_insert(link.origin);
        }
    }
    if windows.is_empty() {
        return Ok(Vec::new());
    }
    let libraries: Vec<String> = windows.keys().cloned().collect();
    let mut parsed: Vec<(window_books::Model, StoredBook)> = Vec::new();
    for library in &libraries {
        let rows = read_paged(
            store,
            window_books::Entity::find()
                .filter(window_books::Column::ParentLibraryId.eq(library.as_str())),
            None,
        )
        .await?;
        let mut books = join_parts(
            read_parts(
                store,
                window_book_parts::Entity::find()
                    .filter(window_book_parts::Column::ParentLibraryId.eq(library.as_str())),
            )
            .await?,
        );
        for row in rows {
            let key = (
                (
                    row.store.clone(),
                    row.marketplace.clone(),
                    row.product_id.clone(),
                ),
                row.parts_version.clone(),
            );
            let Some(book) = books.remove(&key) else {
                continue;
            };
            if book.edition.problem().is_some() {
                continue;
            }
            let identifiers: Vec<LookupIdentifier> = book
                .identifiers
                .into_iter()
                .filter(LookupIdentifier::is_well_formed)
                .collect();
            parsed.push((
                row,
                StoredBook {
                    identifiers,
                    ..book
                },
            ));
        }
    }
    let mut groups = Groups::new(parsed.len());
    let mut first_with: HashMap<String, usize> = HashMap::new();
    for (i, (row, book)) in parsed.iter().enumerate() {
        for key in merge_keys(row, &book.identifiers) {
            match first_with.get(&key) {
                Some(&j) => groups.join(i, j),
                None => {
                    first_with.insert(key, i);
                }
            }
        }
    }
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..parsed.len() {
        let root = groups.root(i);
        members.entry(root).or_default().push(i);
    }
    let mut entries: Vec<WindowEntry> = members
        .into_values()
        .map(|mut indices| {
            indices.sort_by(|&a, &b| {
                let (ra, rb) = (&parsed[a].0, &parsed[b].0);
                (
                    &ra.parent_library_id,
                    &ra.store,
                    &ra.marketplace,
                    &ra.product_id,
                )
                    .cmp(&(
                        &rb.parent_library_id,
                        &rb.store,
                        &rb.marketplace,
                        &rb.product_id,
                    ))
            });
            let work_id = indices
                .iter()
                .find_map(|&i| parsed[i].0.local_work_id.clone());
            let first = &parsed[indices[0]].0;
            let key = work_id.as_ref().map_or_else(
                || {
                    format!(
                        "edition:{}:{}:{}:{}",
                        first.parent_library_id, first.store, first.marketplace, first.product_id
                    )
                },
                |work| format!("work:{work}"),
            );
            let editions = indices
                .iter()
                .map(|&i| {
                    let (row, book) = &parsed[i];
                    WindowEntryEdition {
                        parent_library_id: row.parent_library_id.clone(),
                        owner_origin: windows
                            .get(&row.parent_library_id)
                            .cloned()
                            .unwrap_or_default(),
                        edition: book.edition.clone(),
                        yours: book.yours,
                        copy: book.copy.clone(),
                        work_ref: row.work_ref.clone(),
                    }
                })
                .collect();
            WindowEntry {
                key,
                work_id,
                editions,
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        a.title()
            .to_lowercase()
            .cmp(&b.title().to_lowercase())
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(entries)
}

#[cfg(test)]
#[path = "window_books_tests.rs"]
pub(crate) mod tests;
