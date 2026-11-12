//! Window mode: an install whose link to a library has "Use this computer
//! to download" off is a window of that library. It takes no work from it
//! and instead shows the books that library holds for its person, merged
//! by work with the books of every other library it is a window of.
//!
//! On the library, [`window_books_page`] answers the `window.books` frame:
//! one page of the books on the store accounts linked to the node's owner
//! (the person), grouped by this library's works, each work with its
//! identifiers. Only a branch this library trusts is answered.
//!
//! On the window:
//!
//! 1. [`record_window_page`] stores each page, one `window_books` row per
//!    edition, with the work of this install's own library its identifiers
//!    reach (by identifier, never by title; none when no single work does).
//! 2. [`finish_window_read`] drops the rows a complete read no longer
//!    named, so a book the library stopped holding goes away.
//!    [`clear_window_books`] drops a library's rows outright (download
//!    turned back on, or the link left).
//! 3. [`window_view`] merges every window link's rows into one list: rows
//!    are the same work when they reach the same local work, share a
//!    strong identifier (ASIN with marketplace, OpenLibrary edition or work
//!    id), or are grouped together by the same library.
//!
//! Work ids are local to each library, so a library's work id is only a
//! grouping key inside that library and is never compared across
//! libraries.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bookclerk_node_proto::{
    LookupEdition, LookupFormat, LookupIdentifier, LookupScheme, LookupWork, WindowWork,
    MAX_FRAME_BYTES, MAX_LOOKUP_EDITIONS, MAX_WINDOW_WORKS,
};
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::batch::{exec, text, text_opt};
use super::catalog_lookup::{
    book_edition, identifiers_of, in_chunk, is_trusted_branch, read_chunked_all, read_paged,
    result_page, run_batches, stamp, work_for_question, works_of_editions,
};
use super::cluster_links::list_cluster_links;
use crate::entities::{
    account_links, books, nodes, portal_identities, store_listings, window_books,
};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Most book rows one [`window_books_page`] reads.
pub const WINDOW_BOOKS_READ: u64 = 200;

/// Bytes of works one `window.page` frame carries at most, well under
/// [`MAX_FRAME_BYTES`] so the frame's envelope always fits.
const WINDOW_PAGE_BUDGET: usize = MAX_FRAME_BYTES / 2;

/// Most account ids one read binds, whatever the plugin allows.
const MAX_IN_ACCOUNTS: usize = 64;

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

/// The works of `held` books, by book id: each book's store listing (same
/// plugin key, marketplace and product id), its edition, and that edition's
/// work.
async fn works_of_books(
    store: &LibraryStore,
    held: &[books::Model],
) -> Result<HashMap<i64, String>> {
    let products: Vec<String> = held
        .iter()
        .map(|b| b.product_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // A product id can be listed in several marketplaces: page each chunk.
    let listings = read_chunked_all(store, &products, |part| {
        store_listings::Entity::find().filter(store_listings::Column::ProductId.is_in(part))
    })
    .await?;
    let edition_of: HashMap<(String, String, String), String> = listings
        .into_iter()
        .map(|l| ((l.plugin_key, l.marketplace, l.product_id), l.edition_id))
        .collect();
    let edition_ids: Vec<String> = edition_of
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let work_of_edition = works_of_editions(store, &edition_ids).await?;
    Ok(held
        .iter()
        .filter_map(|b| {
            let key = (
                b.plugin_key.clone(),
                b.marketplace.to_ascii_lowercase(),
                b.product_id.clone(),
            );
            edition_of
                .get(&key)
                .and_then(|edition| work_of_edition.get(edition))
                .map(|work| (b.id, work.clone()))
        })
        .collect())
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

/// One page of the books this library holds for branch `node_id`'s owner,
/// for the `window.books` frame.
///
/// Books are read in id order from cursor `after` (at most
/// [`WINDOW_BOOKS_READ`] rows, and no more than one database result page).
/// Each is grouped under its work here (its key is the work id, or
/// `book-<id>` when it has no work), with the work's identifiers; a work
/// lists at most [`MAX_LOOKUP_EDITIONS`] editions (where two of the
/// person's accounts hold the same edition, one entry with every format
/// either copy has).
/// The page stops early once it would pass the frame budget or
/// [`MAX_WINDOW_WORKS`] works; the books after that come on the next page.
///
/// # Arguments
///
/// * `node_id` - The asking node; its owner is the person.
/// * `after` - The cursor of the previous page (0 for the first).
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
    after: i64,
) -> Result<Option<(Vec<WindowWork>, Option<i64>)>> {
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
    let accounts = person_accounts(store, person).await?;
    if accounts.is_empty() {
        return Ok(Some((Vec::new(), None)));
    }
    // Read by account id in chunks within the plugin's bind limit, then keep
    // only the rows whose plugin key matches too. Each chunk reads at most
    // `limit` rows after the cursor; when a chunk fills, books of that chunk
    // past its last row are unread, so the page ends at the lowest such row
    // and nothing in between is skipped.
    let width = in_chunk(store, 2).min(MAX_IN_ACCOUNTS);
    let account_ids: Vec<String> = accounts
        .iter()
        .map(|(_, account)| account.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let limit = WINDOW_BOOKS_READ.min(u64::try_from(result_page(store)).unwrap_or(1));
    let mut read: Vec<books::Model> = Vec::new();
    let mut boundary: Option<i64> = None;
    for part in account_ids.chunks(width.max(1)) {
        let rows = books::Entity::find()
            .filter(books::Column::AccountId.is_in(part.to_vec()))
            .filter(books::Column::Id.gt(after))
            .order_by_asc(books::Column::Id)
            .limit(limit)
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if u64::try_from(rows.len()).unwrap_or(u64::MAX) >= limit {
            if let Some(last) = rows.last() {
                boundary = Some(boundary.map_or(last.id, |b| b.min(last.id)));
            }
        }
        read.extend(rows);
    }
    read.sort_by_key(|b| b.id);
    read.dedup_by_key(|b| b.id);
    if let Some(boundary) = boundary {
        read.retain(|b| b.id <= boundary);
    }
    let cap = usize::try_from(limit).unwrap_or(usize::MAX);
    let full = boundary.is_some() || read.len() > cap;
    read.truncate(cap);
    let read_last = read.last().map_or(after, |b| b.id);
    let rows: Vec<books::Model> = read
        .into_iter()
        .filter(|b| accounts.contains(&(b.plugin_key.clone(), b.account_id.clone())))
        .collect();
    let work_of_book = works_of_books(store, &rows).await?;
    let work_ids: Vec<String> = work_of_book
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let identifiers = identifiers_of(store, &work_ids).await?;

    let mut works: Vec<WindowWork> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut used = 0usize;
    let mut last = after;
    let mut stopped = false;
    for book in &rows {
        let Some(edition) = book_edition(book) else {
            last = book.id;
            continue;
        };
        let work = work_of_book
            .get(&book.id)
            .cloned()
            .unwrap_or_else(|| format!("book-{}", book.id));
        // A work already listing the most editions a frame allows goes on
        // under another key of this page (`<work>-x<n>`) with the same
        // identifiers, so the window still merges them.
        let mut key = work.clone();
        let mut overflow = 0;
        while let Some(&at) = index.get(&key) {
            let editions = &works[at].editions;
            let listed = editions.iter().any(|e| {
                e.store == edition.store
                    && e.marketplace == edition.marketplace
                    && e.product_id == edition.product_id
            });
            if listed || editions.len() < MAX_LOOKUP_EDITIONS {
                break;
            }
            overflow += 1;
            key = overflow_key(&work, overflow);
        }
        let mut size = encoded_len(&edition);
        let at = index.get(&key).copied();
        if at.is_none() {
            size += encoded_len(&key)
                + identifiers
                    .get(&work)
                    .map_or(0, |ids| ids.iter().map(encoded_len).sum::<usize>());
        }
        let new_work = at.is_none();
        if !works.is_empty()
            && (used + size > WINDOW_PAGE_BUDGET || (new_work && works.len() >= MAX_WINDOW_WORKS))
        {
            stopped = true;
            break;
        }
        last = book.id;
        used += size;
        let at = match at {
            Some(at) => at,
            None => {
                works.push(WindowWork {
                    work: key.clone(),
                    identifiers: identifiers.get(&work).cloned().unwrap_or_default(),
                    editions: Vec::new(),
                });
                index.insert(key, works.len() - 1);
                works.len() - 1
            }
        };
        let editions = &mut works[at].editions;
        let same = editions.iter().position(|e| {
            e.store == edition.store
                && e.marketplace == edition.marketplace
                && e.product_id == edition.product_id
        });
        match same {
            // Two of the person's accounts hold it: the library has every
            // format either copy has.
            Some(i) => {
                let held = &mut editions[i].formats;
                held.extend(edition.formats);
                held.sort_unstable();
                held.dedup();
            }
            None => editions.push(edition),
        }
    }
    if !stopped {
        // Rows of other plugins' accounts were read past too.
        last = read_last;
    }
    let next = (stopped || full)
        .then_some(last)
        .filter(|next| *next > after);
    Ok(Some((works, next)))
}

/// A stamp for one complete read of a library's window books: pass it to
/// every [`record_window_page`] of the read and then to
/// [`finish_window_read`].
#[must_use]
pub fn window_read_stamp() -> String {
    stamp(Utc::now())
}

/// Stores one `window.page` from library `owner_cluster_id`, read under
/// `seen_at` ([`window_read_stamp`]).
///
/// Each edition becomes (or refreshes) one `window_books` row. Each work is
/// placed on a work of this install's library when its identifiers reach
/// exactly one (a strong identifier first, then an ISBN); none otherwise.
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
    owner_cluster_id: &str,
    seen_at: &str,
    works: &[WindowWork],
) -> Result<usize> {
    let earlier = earlier_in_read(store, owner_cluster_id, seen_at, works).await?;
    let mut statements = Vec::new();
    let mut stored = 0;
    for work in works {
        let local = if work.identifiers.is_empty() {
            None
        } else {
            work_for_question(
                store,
                &LookupWork {
                    work: work.work.clone(),
                    identifiers: work.identifiers.clone(),
                },
            )
            .await?
        };
        let identifiers_json = serde_json::to_string(&work.identifiers)
            .map_err(|err| LibraryError::Invalid(err.to_string()))?;
        for edition in &work.editions {
            let key = (
                edition.store.clone(),
                edition.marketplace.clone(),
                edition.product_id.clone(),
            );
            let merged;
            let edition = match earlier.get(&key) {
                Some(formats) => {
                    let mut richer = edition.clone();
                    richer.formats = formats
                        .iter()
                        .chain(&edition.formats)
                        .copied()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    merged = richer;
                    &merged
                }
                None => edition,
            };
            let edition_json = serde_json::to_string(edition)
                .map_err(|err| LibraryError::Invalid(err.to_string()))?;
            statements.push(exec(
                "UPDATE window_books SET work_ref = ?, local_work_id = ?, identifiers_json = ?, \
                 edition_json = ?, seen_at = ? WHERE owner_cluster_id = ? AND store = ? \
                 AND marketplace = ? AND product_id = ?",
                vec![
                    text(grouping_key(&work.work)),
                    text_opt(local.as_deref()),
                    text(&identifiers_json),
                    text(&edition_json),
                    text(seen_at),
                    text(owner_cluster_id),
                    text(&edition.store),
                    text(&edition.marketplace),
                    text(&edition.product_id),
                ],
            ));
            statements.push(exec(
                "INSERT OR IGNORE INTO window_books (owner_cluster_id, store, marketplace, \
                 product_id, work_ref, local_work_id, identifiers_json, edition_json, seen_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(owner_cluster_id),
                    text(&edition.store),
                    text(&edition.marketplace),
                    text(&edition.product_id),
                    text(grouping_key(&work.work)),
                    text_opt(local.as_deref()),
                    text(&identifiers_json),
                    text(&edition_json),
                    text(seen_at),
                ],
            ));
            stored += 1;
        }
    }
    run_batches(store, "window book", statements).await?;
    Ok(stored)
}

/// The formats already stored by this same read (stamped `seen_at`) for the
/// editions of `works`, by `(store, marketplace, product id)`.
///
/// Two of the person's accounts can hold one edition with their books on
/// different pages; the second copy then adds its formats to the first
/// instead of replacing them. A later complete read starts from its own
/// stamp, so a format no longer held still goes.
async fn earlier_in_read(
    store: &LibraryStore,
    owner_cluster_id: &str,
    seen_at: &str,
    works: &[WindowWork],
) -> Result<HashMap<(String, String, String), Vec<LookupFormat>>> {
    let wanted: BTreeSet<(String, String, String)> = works
        .iter()
        .flat_map(|work| &work.editions)
        .map(|e| (e.store.clone(), e.marketplace.clone(), e.product_id.clone()))
        .collect();
    let products: Vec<String> = wanted
        .iter()
        .map(|(_, _, product)| product.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let owner = owner_cluster_id.to_string();
    let stamp_now = seen_at.to_string();
    let rows = read_chunked_all(store, &products, move |part| {
        window_books::Entity::find()
            .filter(window_books::Column::OwnerClusterId.eq(owner.clone()))
            .filter(window_books::Column::SeenAt.eq(stamp_now.clone()))
            .filter(window_books::Column::ProductId.is_in(part))
    })
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let key = (row.store, row.marketplace, row.product_id);
            if !wanted.contains(&key) {
                return None;
            }
            let edition: LookupEdition = serde_json::from_str(&row.edition_json).ok()?;
            Some((key, edition.formats))
        })
        .collect())
}

/// Ends a complete read of library `owner_cluster_id`'s window books: drops
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
    owner_cluster_id: &str,
    seen_at: &str,
) -> Result<u64> {
    run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_books WHERE owner_cluster_id = ? AND seen_at <> ?",
            vec![text(owner_cluster_id), text(seen_at)],
        )],
    )
    .await
}

/// Drops every window book of library `owner_cluster_id` (its link
/// downloads again, or was left).
///
/// # Returns
///
/// How many rows were dropped.
///
/// # Errors
///
/// Returns the write error.
pub async fn clear_window_books(store: &LibraryStore, owner_cluster_id: &str) -> Result<u64> {
    // Most calls find nothing: read first so they write nothing.
    let any = window_books::Entity::find()
        .filter(window_books::Column::OwnerClusterId.eq(owner_cluster_id))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if any.is_none() {
        return Ok(0);
    }
    run_batches(
        store,
        "window book",
        vec![exec(
            "DELETE FROM window_books WHERE owner_cluster_id = ?",
            vec![text(owner_cluster_id)],
        )],
    )
    .await
}

/// One edition of a [`WindowEntry`], and the library that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntryEdition {
    /// The library (owner cluster id) that holds it for the person.
    pub owner_cluster_id: String,
    /// The library's origin, for display.
    pub owner_origin: String,
    /// The edition as that library sent it.
    pub edition: LookupEdition,
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
        row.owner_cluster_id, row.work_ref
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

/// The merged view of every library this install is a window of: the books
/// each holds for the person, one entry per work, sorted by title.
///
/// Only active links with "Use this computer to download" off count. Rows
/// are the same work when they reach the same work of this library, share
/// a strong identifier (not an ISBN), or carry the same grouping key from
/// one library. A row
/// whose stored edition no longer parses is skipped.
///
/// # Errors
///
/// Returns the read error.
pub async fn window_view(store: &LibraryStore) -> Result<Vec<WindowEntry>> {
    let windows: BTreeMap<String, String> = list_cluster_links(store)
        .await?
        .into_iter()
        .filter(|link| link.is_active() && !link.download)
        .map(|link| (link.owner_cluster_id, link.owner_origin))
        .collect();
    if windows.is_empty() {
        return Ok(Vec::new());
    }
    let clusters: Vec<String> = windows.keys().cloned().collect();
    let mut rows = Vec::new();
    for cluster in &clusters {
        rows.extend(
            read_paged(
                store,
                window_books::Entity::find()
                    .filter(window_books::Column::OwnerClusterId.eq(cluster.as_str())),
                None,
            )
            .await?,
        );
    }
    let mut parsed: Vec<(window_books::Model, LookupEdition, Vec<LookupIdentifier>)> = Vec::new();
    for row in rows {
        let Ok(edition) = serde_json::from_str::<LookupEdition>(&row.edition_json) else {
            continue;
        };
        if edition.problem().is_some() {
            continue;
        }
        let identifiers: Vec<LookupIdentifier> =
            serde_json::from_str::<Vec<LookupIdentifier>>(&row.identifiers_json)
                .unwrap_or_default()
                .into_iter()
                .filter(LookupIdentifier::is_well_formed)
                .collect();
        parsed.push((row, edition, identifiers));
    }
    let mut groups = Groups::new(parsed.len());
    let mut first_with: HashMap<String, usize> = HashMap::new();
    for (i, (row, _, identifiers)) in parsed.iter().enumerate() {
        for key in merge_keys(row, identifiers) {
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
                    &ra.owner_cluster_id,
                    &ra.store,
                    &ra.marketplace,
                    &ra.product_id,
                )
                    .cmp(&(
                        &rb.owner_cluster_id,
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
                        first.owner_cluster_id, first.store, first.marketplace, first.product_id
                    )
                },
                |work| format!("work:{work}"),
            );
            let editions = indices
                .iter()
                .map(|&i| {
                    let (row, edition, _) = &parsed[i];
                    WindowEntryEdition {
                        owner_cluster_id: row.owner_cluster_id.clone(),
                        owner_origin: windows
                            .get(&row.owner_cluster_id)
                            .cloned()
                            .unwrap_or_default(),
                        edition: edition.clone(),
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
mod tests;
