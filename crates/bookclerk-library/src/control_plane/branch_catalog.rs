//! Branches of branches: on a library, the titles its branches share with
//! it (`catalog.*` frames; the "Branches of branches" section of the join
//! design).
//!
//! A branch is a library of its own, so joins make a tree (A ← B ← C).
//! Trust stays one hop wide: A knows B only as a host node owned by one of
//! A's users, and never learns of C. B shares the titles of its library
//! (and, unless its operator turned that off, the titles C shared with B)
//! over the node socket. Each title carries its store, product id,
//! marketplace, metadata and file manifest, never an account, token or
//! user field ([`bookclerk_node_proto::CatalogTitle`]). A resolves it into
//! its library model like a scanned listing (work, expression, edition and
//! store listing) and records it as a node holding of B (holder
//! `node:<B>`, [`branch_provenance`]), publishing `holding_added`. The
//! holding carries the share state: the title as received, its manifest
//! digest, and the digest whose files arrived.
//!
//! Every batch is checked on its own, like a host scan's results: the
//! catalog fence is the node's current one ([`begin_catalog`] bumps it),
//! the node is still a branch (kind `host`, live credential, active owner
//! user), and the branch still shares. A title A already holds for that
//! user in its own library is skipped, so nothing B got from A comes back
//! as B's. Files reuse the upload endpoint: [`grant_catalog_upload`] opens
//! an upload bound to the catalog fence (job id
//! [`bookclerk_node_proto::CATALOG_ITEM`]) whose files are the holding's
//! edition's, and its commit records the delivered manifest in one batch
//! gated on that fence. A title whose edition already has audio here from
//! another writer (an acquire, another branch) is not asked for: the
//! cluster delivers each edition once.
//!
//! When a full pass ends, B's holdings it did not name are removed; when B
//! stops sharing or leaves, all of them are, each with `holding_removed`.
//! Files already delivered stay with their editions in A's outputs, and
//! the owner may forget them in bulk ([`remove_detached_branch_titles`]).

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_node_proto::{
    CatalogRef, CatalogTitle, UploadFile, CATALOG_ITEM, MAX_CATALOG_BATCH_TITLES,
    MAX_CATALOG_CURSOR_BYTES, MAX_CATALOG_TITLE_BYTES, MAX_CATALOG_WANTED,
};
use bookclerk_plugin_abi::{DbValue, TypedDbStatement};
use bookclerk_resolve as resolve;
use chrono::Utc;
use sea_orm::sea_query::{Expr, LikeExpr, Query};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, FromQueryResult, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use serde::Serialize;

use super::batch::{self, exec, request, text};
use super::enrollment::load_node_credential;
use super::trust::accounts_owned_by;
use super::uploads::{
    claims_another_books_key, granted_key_count, open_upload_session, upload_holds_its_keys,
    UploadDestination, UploadGrant, UploadGrantRequest, UploadSession,
};
use crate::entities::{
    accounts, books, edition_files, editions, holdings, node_upload_sessions, nodes,
    plugin_versions, store_listings, users,
};
use crate::error::{LibraryError, Result};
use crate::models::UserStatus;
use crate::store::edition_files::like_escaped;
use crate::store::holdings::{find_holdings, share_title_of};
use crate::store::{LibraryStore, NewBook, NodeHoldingSelection};

/// Removal reason: the branch turned "Share this branch's books" off.
pub const DETACHED_STOPPED_SHARING: &str = "stopped_sharing";

/// Removal reason: the branch left the library (or was revoked).
pub const DETACHED_LEFT: &str = "left";

/// Removal reason: a full pass of the branch's catalog no longer named it.
pub const DETACHED_WITHDRAWN: &str = "withdrawn";

/// The provenance text of titles shared by `node_id`: the holder key of
/// its node holdings.
#[must_use]
pub fn branch_provenance(node_id: &str) -> String {
    resolve::Holder::Node(node_id.to_string()).stored()
}

/// The `holdings.share_ref` of a title: `store:product_id` as the branch
/// named it (store ids carry no `:`).
fn share_ref(store: &str, product_id: &str) -> String {
    format!("{store}:{product_id}")
}

/// The store and product id of a `share_ref`.
fn split_share_ref(share_ref: &str) -> (String, String) {
    share_ref.split_once(':').map_or_else(
        || (String::new(), share_ref.to_string()),
        |(store, product)| (store.to_string(), product.to_string()),
    )
}

/// How long a branch's catalog sync resumes incrementally before the
/// library asks for a full pass again, so titles a branch no longer has
/// (removed, or from a sub-branch that left) are withdrawn within a day.
pub const FULL_CATALOG_PASS_EVERY: chrono::Duration = chrono::Duration::hours(24);

/// How long a node holding may carry no share state before a session's
/// start removes it: longer than any catalog batch takes between linking a
/// title and writing its share state.
pub const UNSHARED_GRACE: chrono::Duration = chrono::Duration::minutes(10);

/// True when the branch's last full pass ended long enough ago (and no
/// pass is under way) that the next session starts from the beginning.
fn full_pass_due(row: &nodes::Model) -> bool {
    if row.catalog_pass_started_at.is_some() {
        return false;
    }
    row.catalog_full_pass_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .is_none_or(|at| Utc::now() - at.with_timezone(&Utc) >= FULL_CATALOG_PASS_EVERY)
}

/// RFC 3339 with nanoseconds, so stamps order as text.
fn stamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// The user who owns `node_id` when it is a branch this library still
/// trusts: a `host` node with a live credential and an active owner.
async fn branch_owner(store: &LibraryStore, node_id: &str) -> Result<Option<(nodes::Model, i64)>> {
    let Some(row) = nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    // A clerk (owner node) shares this library's database: it is this
    // library, not a branch, and shares nothing over the socket.
    if row.kind != "host" {
        return Ok(None);
    }
    let live = load_node_credential(store, node_id)
        .await?
        .is_some_and(|credential| credential.revoked_at.is_none());
    let Some(owner) = row.owner_user_id.filter(|_| live) else {
        return Ok(None);
    };
    let active = users::Entity::find_by_id(owner)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some_and(|user| UserStatus::parse(&user.status) == Some(UserStatus::Active));
    Ok(active.then_some((row, owner)))
}

/// An opened catalog session ([`begin_catalog`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSessionOpened {
    /// The new catalog fence.
    pub fence: i64,
    /// Where the branch resumes (empty: from the start).
    pub cursor: String,
    /// Titles recorded earlier whose files have not arrived.
    pub wanted: Vec<CatalogRef>,
}

/// Outcome of [`begin_catalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogBegin {
    /// The session is open under a new fence.
    Opened(CatalogSessionOpened),
    /// The node is not a branch this library trusts.
    Refused(&'static str),
}

/// Opens (or reopens) `node_id`'s catalog sync with the settings the
/// branch reports: records them on its node row, bumps its catalog fence
/// (frames under an older fence are lost from now on), and answers where
/// to resume. A change of either setting restarts the pass from the start,
/// so titles the new setting excludes are withdrawn when the pass ends; so
/// does a session [`FULL_CATALOG_PASS_EVERY`] after the last full pass.
/// With sharing off, every title the branch shared is removed
/// (`stopped_sharing`).
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn begin_catalog(
    store: &LibraryStore,
    node_id: &str,
    share_books: bool,
    include_branches: bool,
) -> Result<CatalogBegin> {
    for _ in 0..8 {
        let Some((row, _owner)) = branch_owner(store, node_id).await? else {
            return Ok(CatalogBegin::Refused("not_a_branch"));
        };
        let changed = row.share_books != Some(i64::from(share_books))
            || row.include_branches != Some(i64::from(include_branches));
        let cursor = if changed || !share_books || full_pass_due(&row) {
            String::new()
        } else {
            row.catalog_cursor.clone()
        };
        let pass_started_at = if cursor.is_empty() {
            Some(stamp())
        } else {
            row.catalog_pass_started_at.clone()
        };
        let fence = row.catalog_fence.saturating_add(1);
        let updated = nodes::Entity::update_many()
            .col_expr(
                nodes::Column::ShareBooks,
                Expr::value(i64::from(share_books)),
            )
            .col_expr(
                nodes::Column::IncludeBranches,
                Expr::value(i64::from(include_branches)),
            )
            .col_expr(nodes::Column::CatalogFence, Expr::value(fence))
            .col_expr(nodes::Column::CatalogCursor, Expr::value(cursor.clone()))
            .col_expr(
                nodes::Column::CatalogPassStartedAt,
                Expr::value(pass_started_at),
            )
            .filter(nodes::Column::NodeId.eq(node_id))
            .filter(nodes::Column::CatalogFence.eq(row.catalog_fence))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if updated.rows_affected != 1 {
            continue;
        }
        let wanted = if share_books {
            // Holdings a batch linked under a fence that was gone by the
            // time it wrote carry no share state; once old enough that no
            // batch can still be writing them, they go.
            // `created_at` is written as `to_rfc3339`, so the cutoff is too.
            let cutoff = (Utc::now() - UNSHARED_GRACE).to_rfc3339();
            store
                .remove_node_holdings(node_id, NodeHoldingSelection::UnsharedBefore(&cutoff))
                .await?;
            pending_deliveries(store, node_id, MAX_CATALOG_WANTED).await?
        } else {
            // Guarded on this connection's fence: a newer connection that
            // shares again keeps what it links meanwhile.
            detach(
                store,
                node_id,
                NodeHoldingSelection::AllUnderFence(fence),
                DETACHED_STOPPED_SHARING,
            )
            .await?;
            Vec::new()
        };
        tracing::info!(
            node_id,
            fence,
            share_books,
            include_branches,
            resume = !cursor.is_empty(),
            "opened a branch's catalog sync"
        );
        return Ok(CatalogBegin::Opened(CatalogSessionOpened {
            fence,
            cursor,
            wanted,
        }));
    }
    Err(LibraryError::Unavailable(
        "the branch's catalog fence kept changing".into(),
    ))
}

/// The node row and owner when `fence` is `node_id`'s live catalog fence,
/// the node is still a trusted branch, and it still shares.
async fn live_catalog(
    store: &LibraryStore,
    node_id: &str,
    fence: i64,
) -> Result<Option<(nodes::Model, i64)>> {
    Ok(branch_owner(store, node_id)
        .await?
        .filter(|(row, _)| row.catalog_fence == fence && fence > 0 && row.share_books == Some(1)))
}

/// True while `fence` is `node_id`'s live catalog fence and it may share.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn catalog_fence_live(store: &LibraryStore, node_id: &str, fence: i64) -> Result<bool> {
    Ok(live_catalog(store, node_id, fence).await?.is_some())
}

/// Outcome of [`accept_catalog_batch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogAccept {
    /// The batch was recorded.
    Accepted {
        /// Titles recorded.
        upserted: u64,
        /// Titles skipped because this library already holds them for the
        /// branch's owner (no echo back), or because they formed no valid
        /// listing.
        skipped: u64,
        /// Titles whose files this library wants.
        wanted: Vec<CatalogRef>,
    },
    /// The fence is not live (or the branch no longer shares or is no
    /// longer trusted); nothing was recorded.
    Lost,
    /// The batch is not allowed; nothing was recorded.
    Refused(&'static str),
}

/// True for a cursor: bounded text without control characters.
fn is_cursor(cursor: &str) -> bool {
    cursor.len() <= MAX_CATALOG_CURSOR_BYTES && !cursor.chars().any(char::is_control)
}

/// The batch's shape problem, if any: the size bounds, each title's own
/// checks, and no title twice.
fn batch_problem(cursor: &str, titles: &[CatalogTitle]) -> Option<&'static str> {
    if titles.len() > MAX_CATALOG_BATCH_TITLES {
        return Some("batch_too_large");
    }
    if !is_cursor(cursor) {
        return Some("invalid_cursor");
    }
    let mut seen = BTreeSet::new();
    for title in titles {
        if serde_json::to_string(title).map_or(usize::MAX, |text| text.len())
            > MAX_CATALOG_TITLE_BYTES
        {
            return Some("title_too_large");
        }
        if let Some(problem) = title.problem() {
            return Some(problem);
        }
        if !seen.insert(title.reference()) {
            return Some("duplicate_title");
        }
    }
    None
}

/// True when this library already holds `title` for `owned` (its own
/// store rows of the branch's owner): the branch got it from here.
async fn held_here(
    store: &LibraryStore,
    owned: &BTreeSet<(String, String)>,
    title: &CatalogTitle,
) -> Result<bool> {
    if owned.is_empty() {
        return Ok(false);
    }
    let rows = books::Entity::find()
        .filter(books::Column::ProductId.eq(&title.product_id))
        .filter(books::Column::Source.eq(&title.store))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows
        .iter()
        .any(|row| owned.contains(&(row.plugin_key.clone(), row.account_id.clone()))))
}

/// The plugin key this library files `store`'s listings under: the plugin
/// whose alias is `store` (a connected account's, else an installed
/// version's), or the store id itself when this library has no such
/// plugin. Editions still meet across plugin keys through their ASIN and
/// ISBN.
async fn listing_plugin_key(store: &LibraryStore, store_id: &str) -> Result<String> {
    if let Some(account) = accounts::Entity::find()
        .filter(accounts::Column::Source.eq(store_id))
        .order_by_asc(accounts::Column::PluginKey)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    {
        return Ok(account.plugin_key);
    }
    Ok(plugin_versions::Entity::find()
        .filter(plugin_versions::Column::ManifestId.eq(store_id))
        .order_by_asc(plugin_versions::Column::PluginKey)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map_or_else(|| store_id.to_string(), |version| version.plugin_key))
}

/// The listing a shared title describes, filed under `plugin_key`: its
/// metadata, ASIN scoped to its marketplace, ISBN, and media kind from
/// its content kind.
///
/// # Errors
///
/// Returns [`resolve::ListingError`] when the title fails listing
/// validation.
pub fn listing_from_catalog_title(
    title: &CatalogTitle,
    plugin_key: &str,
) -> std::result::Result<resolve::Listing, resolve::ListingError> {
    let meta = &title.metadata;
    let published_at = meta
        .published_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc));
    let book = NewBook {
        uuid: None,
        product_id: title.product_id.clone(),
        plugin_key: plugin_key.to_string(),
        source: title.store.clone(),
        account_id: String::new(),
        asin: meta.asin.clone(),
        isbn: meta.isbn.clone(),
        marketplace: title.marketplace.clone(),
        title: meta.title.clone(),
        authors: meta.authors.clone(),
        narrators: meta.narrators.clone(),
        series: meta.series.clone(),
        series_index: meta.series_index.clone(),
        series_asin: None,
        purchased_at: None,
        publisher: meta.publisher.clone(),
        length_minutes: meta.length_minutes,
        is_abridged: false,
        content_kind: meta
            .content_kind
            .clone()
            .unwrap_or_else(|| String::from("book")),
        categories: None,
        subtitle: meta.subtitle.clone(),
        published_at,
    };
    crate::store::library_model::listing_from_book(&book)
}

/// The gate every statement of a catalog batch carries: `node_id`'s
/// catalog fence is still the batch's, the branch still shares, and its
/// credential is live. Binds: [`live_catalog_binds`].
const LIVE_CATALOG_GATE: &str = "EXISTS (SELECT 1 FROM nodes WHERE node_id = ? \
       AND catalog_fence = ? AND share_books = 1) \
     AND EXISTS (SELECT 1 FROM node_credentials \
       WHERE node_id = ? AND revoked_at IS NULL)";

/// The binds of [`LIVE_CATALOG_GATE`].
fn live_catalog_binds(node_id: &str, fence: i64) -> Vec<DbValue> {
    vec![text(node_id), batch::int(fence), text(node_id)]
}

/// One catalog batch read and checked, ready to write in one gated
/// transaction ([`apply_catalog_batch`]).
struct PreparedCatalogBatch {
    /// The batch's operation id.
    operation_id: String,
    /// The cursor move first, then one statement per recorded title, then
    /// one per title skipped as held here (marking its holding seen).
    statements: Vec<TypedDbStatement>,
    /// Title statements in `statements` (after the cursor move).
    titles: usize,
    /// Titles skipped because this library already holds them, or because
    /// they formed no valid listing.
    skipped: u64,
    /// Titles whose files this library wants.
    wanted: Vec<CatalogRef>,
    /// The full pass this batch ends, when it is the last batch of one.
    ends_pass: Option<String>,
    /// The branch's other holdings of titles this batch linked: a title
    /// that now resolves to another listing (its listing plugin or
    /// marketplace changed) leaves its earlier holding behind, which goes
    /// once the batch is written.
    replaced: Vec<String>,
}

/// Records one batch of `node_id`'s shared titles under `fence`, then the
/// cursor after it. Every title is checked before any is written; a title
/// this library already holds for the branch's owner is skipped. Each
/// other title is resolved into the library model and held by the branch
/// (`holding_added` when new), and the holding's share state is written
/// in one transaction with the cursor, each statement gated on the fence
/// still being live, the branch still sharing and its credential live. A
/// holding is stamped with the pass under way, and with `done` (the last
/// batch of a pass) while a pass from the start is under way, the
/// branch's holdings the pass did not name are removed (`withdrawn`) and
/// the pass ends; `done` on a resumed incremental pass changes nothing
/// more.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn accept_catalog_batch(
    store: &LibraryStore,
    node_id: &str,
    fence: i64,
    cursor: &str,
    done: bool,
    titles: &[CatalogTitle],
) -> Result<CatalogAccept> {
    match prepare_catalog_batch(store, node_id, fence, cursor, done, titles).await? {
        Ok(prepared) => apply_catalog_batch(store, node_id, fence, prepared).await,
        Err(outcome) => Ok(outcome),
    }
}

/// Reads and checks one batch ([`accept_catalog_batch`]), linking each
/// recorded title's listing and node holding: the batch's statements, or
/// the outcome when nothing is to be written.
async fn prepare_catalog_batch(
    store: &LibraryStore,
    node_id: &str,
    fence: i64,
    cursor: &str,
    done: bool,
    titles: &[CatalogTitle],
) -> Result<std::result::Result<PreparedCatalogBatch, CatalogAccept>> {
    if let Some(problem) = batch_problem(cursor, titles) {
        return Ok(Err(CatalogAccept::Refused(problem)));
    }
    let Some((row, owner)) = live_catalog(store, node_id, fence).await? else {
        return Ok(Err(CatalogAccept::Lost));
    };
    let owned = accounts_owned_by(store, owner).await?;
    let holder = resolve::Holder::Node(node_id.to_string());
    let held_by_node = resolve::HoldingDraft {
        holder: holder.clone(),
        entitlement: resolve::Entitlement::Purchase,
        purchased_at: None,
    };
    let pass = row.catalog_pass_started_at.clone();
    let now = stamp();
    let gated = |sql: &str, mut binds: Vec<DbValue>| {
        binds.extend(live_catalog_binds(node_id, fence));
        exec(&format!("{sql} AND {LIVE_CATALOG_GATE}"), binds)
    };
    // The cursor moves only under the live fence.
    let mut statements = vec![gated(
        "UPDATE nodes SET catalog_cursor = ?, catalog_synced_at = ? WHERE node_id = ?",
        vec![text(cursor), text(&now), text(node_id)],
    )];
    let mut skipped = 0u64;
    let mut wanted = Vec::new();
    let mut plugin_keys: BTreeMap<String, String> = BTreeMap::new();
    // A title skipped as held here is still one the branch named: a
    // holding recorded for it earlier is stamped with the pass, so the
    // pass's end does not withdraw it as gone from the branch.
    let mut held_seen = Vec::new();
    let mut linked: BTreeMap<String, String> = BTreeMap::new();
    for title in titles {
        let reference = share_ref(&title.store, &title.product_id);
        if held_here(store, &owned, title).await? {
            skipped += 1;
            if let Some(pass) = &pass {
                held_seen.push(gated(
                    "UPDATE holdings SET seen_pass = ? WHERE node_id = ? AND share_ref = ?",
                    vec![text(pass), text(node_id), text(&reference)],
                ));
            }
            continue;
        }
        let plugin_key = match plugin_keys.get(&title.store) {
            Some(key) => key.clone(),
            None => {
                let key = listing_plugin_key(store, &title.store).await?;
                plugin_keys.insert(title.store.clone(), key.clone());
                key
            }
        };
        let listing = match listing_from_catalog_title(title, &plugin_key) {
            Ok(listing) => listing,
            Err(err) => {
                tracing::warn!(
                    node_id,
                    store = %title.store,
                    product_id = %title.product_id,
                    error = %err,
                    "a shared title forms no listing; skipped"
                );
                skipped += 1;
                continue;
            }
        };
        // The link is written ahead of the gated batch (the link batch
        // checks sharing and the credential, not the catalog fence). When
        // the batch then finds its fence gone (`Lost`), a holding its link
        // created is left without share state; the next session's start
        // removes such holdings once no batch can still be writing them
        // ([`UNSHARED_GRACE`]).
        let plan = store.record_listing(&listing, &held_by_node).await?;
        let Some(holding) = find_holdings()
            .filter(holdings::Column::ListingId.eq(&plan.listing.id))
            .filter(holdings::Column::Holder.eq(holder.stored()))
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            // The branch stopped sharing, left or was revoked since the
            // read above: the link wrote no holding.
            return Ok(Err(CatalogAccept::Lost));
        };
        linked.insert(reference.clone(), holding.id.clone());
        let digest = title.manifest_digest();
        let shared = serde_json::to_string(title)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("catalog title: {err}")))?;
        statements.push(gated(
            "UPDATE holdings SET share_ref = ?, share_title = ?, share_digest = ?, \
               seen_pass = COALESCE(?, seen_pass), updated_at = ? \
              WHERE id = ?",
            vec![
                text(&reference),
                text(&shared),
                text(&digest),
                batch::text_opt(pass.as_deref()),
                text(&now),
                text(&holding.id),
            ],
        ));
        if holding.delivered_digest.as_deref() != Some(digest.as_str())
            && wanted.len() < MAX_CATALOG_WANTED
            && !edition_has_other_audio(store, node_id, &plan.listing.edition_id, &holding.id)
                .await?
        {
            wanted.push(title.reference());
        }
    }
    let recorded = statements.len() - 1;
    statements.extend(held_seen);
    let replaced = replaced_holdings(store, node_id, &linked).await?;
    Ok(Ok(PreparedCatalogBatch {
        operation_id: format!("catalog-batch:{node_id}:{fence}:{now}"),
        statements,
        titles: recorded,
        skipped,
        wanted,
        ends_pass: pass.filter(|_| done),
        replaced,
    }))
}

/// The node's holdings that share a reference with a holding this batch
/// linked (`linked` maps each reference to the holding kept for it) but
/// are not that holding: ones a title's newer listing replaced. The
/// references are read in chunks that fit the backend's binds and
/// statement size, and each chunk in keyset pages of at most one result
/// page.
///
/// # Errors
///
/// Returns the read error.
async fn replaced_holdings(
    store: &LibraryStore,
    node_id: &str,
    linked: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let page_rows = super::trust::result_page(store);
    let query = |refs: &[String], after: &str| {
        holdings::Entity::find()
            .select_only()
            .column(holdings::Column::Id)
            .column(holdings::Column::ShareRef)
            .filter(holdings::Column::HolderKind.eq("node"))
            .filter(holdings::Column::NodeId.eq(node_id))
            .filter(holdings::Column::ShareRef.is_in(refs.iter().cloned()))
            .filter(holdings::Column::Id.gt(after.to_string()))
            .order_by_asc(holdings::Column::Id)
            .limit(page_rows)
    };
    let refs: Vec<String> = linked.keys().cloned().collect();
    let fits = |part: &[String]| {
        super::sea_exec(&query(part, "")).map_or(true, |stmt| store.admits_read(&stmt))
    };
    let mut replaced = Vec::new();
    for chunk in crate::store::holdings::fitting_chunks(&refs, store.lookup_chunk(), fits) {
        let mut after = String::new();
        loop {
            let page = query(chunk, &after)
                .into_model::<RefRow>()
                .all(store.db())
                .await
                .map_err(LibraryError::Orm)?;
            let Some(last) = page.last() else {
                break;
            };
            after.clone_from(&last.id);
            let full = u64::try_from(page.len()).unwrap_or(u64::MAX) >= page_rows;
            for row in page {
                let kept = row.share_ref.as_ref().and_then(|r| linked.get(r));
                if kept.is_some_and(|kept| *kept != row.id) {
                    replaced.push(row.id);
                }
            }
            if !full {
                break;
            }
        }
    }
    Ok(replaced)
}

/// Writes a prepared batch in one transaction ([`CatalogAccept::Lost`],
/// nothing recorded, when its gate no longer holds), then, when it ends a
/// full pass, removes the branch's holdings the pass did not name and
/// ends the pass.
async fn apply_catalog_batch(
    store: &LibraryStore,
    node_id: &str,
    fence: i64,
    prepared: PreparedCatalogBatch,
) -> Result<CatalogAccept> {
    let reply =
        batch::execute_host_batch(store, request(&prepared.operation_id, prepared.statements))
            .await?;
    if batch::rows_affected(&reply, 0) != 1 {
        return Ok(CatalogAccept::Lost);
    }
    let upserted = (1..=prepared.titles)
        .map(|index| batch::rows_affected(&reply, index))
        .sum();
    if !prepared.replaced.is_empty() {
        store
            .remove_node_holdings(
                node_id,
                NodeHoldingSelection::Replaced {
                    ids: &prepared.replaced,
                    fence,
                },
            )
            .await?;
    }
    // Ending the pass follows the batch rather than joining it: the
    // removal is paged and may take several batches. When a step fails
    // the pass stays under way and the branch's next sync, which ends
    // with another `done` batch, ends it again.
    if let Some(pass) = prepared.ends_pass {
        // Each removal is guarded on this pass still being the node's, so a
        // pass started over meanwhile keeps what it stamps.
        let selected = store
            .remove_node_holdings(node_id, NodeHoldingSelection::UnseenBy(&pass))
            .await?;
        if selected > 0 {
            tracing::info!(
                node_id,
                selected,
                reason = DETACHED_WITHDRAWN,
                "a branch's full catalog pass withdrew titles it no longer names"
            );
        }
        let now = stamp();
        let mut binds = vec![text(&now), text(node_id), text(&pass)];
        binds.extend(live_catalog_binds(node_id, fence));
        batch::execute_host_batch(
            store,
            request(
                &format!("catalog-pass-end:{node_id}:{fence}:{now}"),
                vec![exec(
                    &format!(
                        "UPDATE nodes SET catalog_pass_started_at = NULL, \
                           catalog_full_pass_at = ? \
                          WHERE node_id = ? AND catalog_pass_started_at = ? \
                            AND {LIVE_CATALOG_GATE}"
                    ),
                    binds,
                )],
            ),
        )
        .await?;
    }
    Ok(CatalogAccept::Accepted {
        upserted,
        skipped: prepared.skipped,
        wanted: prepared.wanted,
    })
}

/// True when `edition_id` already has audio in this library's outputs
/// that `node_id` did not deliver: an acquire's, another account's upload,
/// or another branch's delivery. Such a title's files are not asked for
/// again. The branch's own catalog uploads never count against it, also
/// those of an earlier holding of the same title (one removed when the
/// branch stopped sharing and held again since, under a new id).
async fn edition_has_other_audio(
    store: &LibraryStore,
    node_id: &str,
    edition_id: &str,
    holding_id: &str,
) -> Result<bool> {
    let found = edition_files::Entity::find()
        .filter(edition_files::Column::OwnerKind.eq("edition"))
        .filter(edition_files::Column::OwnerId.eq(edition_id))
        .filter(edition_files::Column::Role.eq(crate::store::FileRole::Audio.as_str()))
        .filter(
            edition_files::Column::Writer.not_in_subquery(
                Query::select()
                    .column(node_upload_sessions::Column::UploadId)
                    .from(node_upload_sessions::Entity)
                    .and_where(node_upload_sessions::Column::BookUuid.eq(holding_id))
                    .to_owned(),
            ),
        )
        .filter(edition_files::Column::Writer.not_in_subquery(catalog_uploads_of(node_id)))
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(found > 0)
}

/// Most node holdings one page of [`pending_deliveries`] reads.
const PENDING_PAGE: u64 = 200;

/// Up to `limit` of `node_id`'s shared titles whose files have not
/// arrived (or changed since), leaving out titles whose edition already
/// has audio here from another writer.
///
/// # Errors
///
/// Returns the read error.
pub async fn pending_deliveries(
    store: &LibraryStore,
    node_id: &str,
    limit: usize,
) -> Result<Vec<CatalogRef>> {
    // A title whose upload is refused for good (`key_in_use`,
    // `key_of_another_book`, `folder_of_another_book`) is still asked for
    // on every session: no per-title refusal is recorded to skip it by.
    let mut out = Vec::new();
    let mut after = String::new();
    let page_rows = PENDING_PAGE.min(super::trust::result_page(store));
    while out.len() < limit {
        let page = find_holdings()
            .filter(holdings::Column::HolderKind.eq("node"))
            .filter(holdings::Column::NodeId.eq(node_id))
            .filter(holdings::Column::ShareDigest.is_not_null())
            .filter(holdings::Column::Id.gt(after.clone()))
            .order_by_asc(holdings::Column::Id)
            .limit(page_rows)
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let Some(last) = page.last() else {
            break;
        };
        after.clone_from(&last.id);
        let full = u64::try_from(page.len()).unwrap_or(u64::MAX) >= page_rows;
        for row in page {
            if out.len() >= limit || row.delivered_digest == row.share_digest {
                continue;
            }
            let Some(reference) = row.share_ref.as_deref() else {
                continue;
            };
            let edition = store_listings::Entity::find_by_id(row.listing_id.clone())
                .one(store.db())
                .await
                .map_err(LibraryError::Orm)?;
            if let Some(listing) = edition {
                if edition_has_other_audio(store, node_id, &listing.edition_id, &row.id).await? {
                    continue;
                }
            }
            let (store_id, product_id) = split_share_ref(reference);
            out.push(CatalogRef {
                store: store_id,
                product_id,
            });
        }
        if !full {
            break;
        }
    }
    Ok(out)
}

/// The node holding through which `node_id` shares the title it names
/// `store_id`/`product_id`, with its edition and the title as shared.
async fn shared_holding(
    store: &LibraryStore,
    node_id: &str,
    store_id: &str,
    product_id: &str,
) -> Result<Option<(holdings::Model, String, CatalogTitle)>> {
    let Some(row) = find_holdings()
        .filter(holdings::Column::HolderKind.eq("node"))
        .filter(holdings::Column::NodeId.eq(node_id))
        .filter(holdings::Column::ShareRef.eq(share_ref(store_id, product_id)))
        .filter(holdings::Column::ShareDigest.is_not_null())
        .order_by_asc(holdings::Column::Id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    let Some(title) = share_title_of(store, &row.id)
        .await?
        .and_then(|text| serde_json::from_str::<CatalogTitle>(&text).ok())
    else {
        return Ok(None);
    };
    let Some(listing) = store_listings::Entity::find_by_id(row.listing_id.clone())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    Ok(Some((row, listing.edition_id, title)))
}

/// Opens an upload of one shared title's files, bound to `node_id`'s
/// catalog `fence` and received by `receiving_node_id`.
///
/// Granted only while the fence is live, for a title the branch holds
/// here, when every file is copied to each of `destinations` (this node's,
/// the primary first) under its manifest key, the files are exactly the
/// shared manifest, the title's edition has no audio here from another
/// writer (`edition_has_files`), no key is one another title here already
/// uses (`key_in_use`: another edition's file, or a file beside one of its
/// audio files with the same stem; `key_of_another_book`: any of a
/// library book's keys or their sidecars), and no key lies in or below a
/// folder holding another title's audio (`folder_of_another_book`). The
/// keys are then claimed for this upload as the edition's in
/// `edition_files`, refused when another title holds one there.
///
/// # Errors
///
/// Returns an error when a read or the insert fails.
#[allow(clippy::too_many_arguments)]
pub async fn grant_catalog_upload(
    store: &LibraryStore,
    node_id: &str,
    fence: i64,
    receiving_node_id: &str,
    store_id: &str,
    product_id: &str,
    files: &[UploadFile],
    destinations: &[String],
) -> Result<UploadGrant> {
    if live_catalog(store, node_id, fence).await?.is_none() {
        return Ok(UploadGrant::Lost);
    }
    let Some((row, edition_id, shared)) =
        shared_holding(store, node_id, store_id, product_id).await?
    else {
        return Ok(UploadGrant::Refused("unknown_title"));
    };
    let declared: BTreeMap<&str, u64> = shared
        .files
        .iter()
        .map(|file| (file.key.as_str(), file.size))
        .collect();
    let mut offered = BTreeMap::new();
    for file in files {
        // One key on every destination: a catalog title keeps the
        // branch's layout.
        let mut keys: BTreeSet<&str> = file.copies.iter().map(|copy| copy.key.as_str()).collect();
        let (Some(key), true) = (keys.pop_first(), keys.is_empty()) else {
            return Ok(UploadGrant::Refused("invalid_manifest"));
        };
        if file.copies.len() != destinations.len() || offered.insert(key, file.size).is_some() {
            return Ok(UploadGrant::Refused("invalid_manifest"));
        }
    }
    if offered != declared {
        return Ok(UploadGrant::Refused("manifest_changed"));
    }
    if edition_has_other_audio(store, node_id, &edition_id, &row.id).await? {
        return Ok(UploadGrant::Refused("edition_has_files"));
    }
    let keys: Vec<String> = declared.keys().map(|key| (*key).to_string()).collect();
    if keys_in_use(store, &edition_id, &shared.primary_key, &keys).await? {
        return Ok(UploadGrant::Refused("key_in_use"));
    }
    // Any declared key, not only the primary's stem: a sidecar such as
    // another title's cover, `.cue`, chapter JSON or metadata file, or that
    // title's audio or PDF itself, even where no `edition_files` record says
    // who wrote it.
    if claims_another_books_key(store, &BTreeSet::from([row.id.clone()]), files).await? {
        return Ok(UploadGrant::Refused("key_of_another_book"));
    }
    // The branch's own keys are the title's scope on every destination:
    // the folders they lie in, each key checked against the shared
    // manifest above (and claimed for this upload when it opens).
    let title_folders: Vec<String> = declared
        .keys()
        .map(|key| key_folder(key).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // A folder another title's audio lies in is that title's: a branch
    // writes nothing there or below it, whatever the file is named.
    if folders_of_another_title(store, &edition_id, node_id, &title_folders).await? {
        return Ok(UploadGrant::Refused("folder_of_another_book"));
    }
    let scoped: Vec<UploadDestination> = destinations
        .iter()
        .map(|id| UploadDestination {
            id: id.clone(),
            title_folders: title_folders.clone(),
            title_names: None,
        })
        .collect();
    let digest = row.share_digest.clone().unwrap_or_default();
    let request = UploadGrantRequest {
        book_uuid: &row.id,
        primary_key: &shared.primary_key,
        pdf_key: shared.pdf_key.as_deref(),
        files,
        destinations: &scoped,
    };
    open_upload_session(
        store,
        node_id,
        CATALOG_ITEM,
        fence,
        receiving_node_id,
        &request,
        Some(&digest),
    )
    .await
}

/// A `LIKE` pattern for keys beside `primary` sharing its stem
/// (`dir/stem.%`), wildcards in the key escaped with `\`. `LIKE` compares
/// case-sensitively on Postgres, so there a stem differing only in case
/// does not match.
fn stem_pattern(primary: &str) -> String {
    let (dir, name) = primary
        .rsplit_once('/')
        .map_or(("", primary), |(d, n)| (d, n));
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let base = if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    };
    let mut pattern = like_escaped(&base);
    pattern.push_str(".%");
    pattern
}

/// The folder `key` lies in (everything before its last `/`); `""` for a
/// key at the top of the outputs.
fn key_folder(key: &str) -> &str {
    key.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The condition that `column` (a storage key) lies directly in `folder`,
/// not in a folder below it: `folder/%` but not `folder/%/%`, or, for the
/// top of the outputs (`""`), no `/` at all.
fn directly_in<C: ColumnTrait>(column: C, folder: &str) -> Condition {
    if folder.is_empty() {
        return Condition::all().add(column.not_like("%/%"));
    }
    let base = like_escaped(folder);
    Condition::all()
        .add(column.like(LikeExpr::new(format!("{base}/%")).escape('\\')))
        .add(column.not_like(LikeExpr::new(format!("{base}/%/%")).escape('\\')))
}

/// Folder probes per read of [`folders_of_another_title`], keeping each
/// read's binds well under the smallest guest bind cap.
const FOLDER_PROBES_PER_READ: usize = 32;

/// The folders a catalog title's `folders` (the folders its keys lie in)
/// are probed for another title's audio at: each folder itself (the top of
/// the outputs, `""`, included), and every non-empty folder above it
/// (`A/Book/Extras` → `A/Book/Extras`, `A/Book`, `A`), each once.
fn folder_probes(folders: &[String]) -> BTreeSet<&str> {
    let mut probes = BTreeSet::new();
    for folder in folders {
        probes.insert(folder.as_str());
        let mut above = folder.as_str();
        while let Some((parent, _)) = above.rsplit_once('/') {
            if !parent.is_empty() {
                probes.insert(parent);
            }
            above = parent;
        }
    }
    probes
}

/// The condition that a file record is another live title's: an edition
/// in the library other than `edition_id`.
fn another_live_edition(edition_id: &str) -> Condition {
    Condition::all()
        .add(edition_files::Column::OwnerKind.eq("edition"))
        .add(edition_files::Column::OwnerId.ne(edition_id))
        .add(
            edition_files::Column::OwnerId.in_subquery(
                Query::select()
                    .column(editions::Column::Id)
                    .from(editions::Entity)
                    .to_owned(),
            ),
        )
}

/// The upload ids of `node_id`'s catalog uploads.
fn catalog_uploads_of(node_id: &str) -> sea_orm::sea_query::SelectStatement {
    Query::select()
        .column(node_upload_sessions::Column::UploadId)
        .from(node_upload_sessions::Entity)
        .and_where(node_upload_sessions::Column::NodeId.eq(node_id))
        .and_where(node_upload_sessions::Column::JobId.eq(CATALOG_ITEM))
        .to_owned()
}

/// True when one of `folders`, or a folder above one of them, directly
/// holds the primary audio of another title in this library: one of its
/// books (whatever its owner, in any state), or any file key `edition_files`
/// records there for another edition (one an acquire wrote, or another
/// branch's delivered or granted title). A catalog upload writes into no
/// folder another title lives in, nor below one, so no declared name,
/// however chosen, can replace that title's files, recorded in
/// `edition_files` or not.
///
/// `node_id`'s own titles never hold a folder against it: a branch may
/// lay several of its titles out in one folder (a flat
/// `{Author}/{Title}.m4b` template, or podcast episodes saved together),
/// and the key checks keep their files apart. Another branch's title holds
/// a folder only once one of its uploads claimed a key there, so a
/// declared title whose files never arrive (one refused here, say) squats
/// no folder.
///
/// The folder match is `LIKE`, which compares case-sensitively on
/// Postgres (and case-insensitively for ASCII on SQLite): on Postgres a
/// folder differing from another title's only in case is not that title's.
///
/// # Errors
///
/// Returns an error when a read fails.
async fn folders_of_another_title(
    store: &LibraryStore,
    edition_id: &str,
    node_id: &str,
    folders: &[String],
) -> Result<bool> {
    let probes: Vec<&str> = folder_probes(folders).into_iter().collect();
    for chunk in probes.chunks(FOLDER_PROBES_PER_READ) {
        let books_here = chunk.iter().fold(Condition::any(), |any, folder| {
            any.add(directly_in(books::Column::StorageKey, folder))
        });
        let books = books::Entity::find()
            .filter(books::Column::StorageKey.is_not_null())
            .filter(books_here)
            .count(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if books > 0 {
            return Ok(true);
        }
        // Any file another live title wrote or was granted there holds the
        // folder too, so of two titles reaching one folder before either
        // is delivered, whoever claims a key in it first keeps it.
        let files_here = chunk.iter().fold(Condition::any(), |any, folder| {
            any.add(directly_in(edition_files::Column::StorageKey, folder))
        });
        let files = edition_files::Entity::find()
            .filter(another_live_edition(edition_id))
            .filter(edition_files::Column::Writer.not_in_subquery(catalog_uploads_of(node_id)))
            .filter(files_here)
            .count(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if files > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when a catalog title's files would land on a key another title
/// already uses in this library: one of this library's own books (its
/// audio, its PDF, or a file beside its audio with the same stem), or
/// another edition's recorded file (its audio or any other key, or a file
/// beside the primary with the same stem). A branch's keys never
/// overwrite what is already in the outputs.
async fn keys_in_use(
    store: &LibraryStore,
    edition_id: &str,
    primary: &str,
    keys: &[String],
) -> Result<bool> {
    let pattern = stem_pattern(primary);
    let books = books::Entity::find()
        .filter(
            Condition::any()
                .add(books::Column::StorageKey.is_in(keys.iter().cloned()))
                .add(books::Column::PdfStorageKey.is_in(keys.iter().cloned()))
                .add(books::Column::StorageKey.like(LikeExpr::new(pattern.clone()).escape('\\'))),
        )
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if books > 0 {
        return Ok(true);
    }
    let files = edition_files::Entity::find()
        .filter(another_live_edition(edition_id))
        .filter(
            Condition::any()
                .add(edition_files::Column::StorageKey.is_in(keys.iter().cloned()))
                .add(
                    Condition::all()
                        .add(edition_files::Column::Role.eq(crate::store::FileRole::Audio.as_str()))
                        .add(
                            edition_files::Column::StorageKey
                                .like(LikeExpr::new(pattern).escape('\\')),
                        ),
                ),
        )
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(files > 0)
}

/// Commits a catalog upload whose every file arrived: records the
/// delivered manifest on the node holding. One batch, each statement
/// gated on the upload being open, the catalog fence still being the
/// node's and its sharing on, its credential live, the upload still
/// holding every key it was granted (`edition_files`; a key another title
/// took since is refused as `key_taken`), and the title's manifest
/// unchanged since the grant. A concurrent identical commit that landed
/// first answers committed.
///
/// # Errors
///
/// Returns an error when a read or the batch fails.
pub(super) async fn commit_catalog_upload(
    store: &LibraryStore,
    session: &UploadSession,
) -> Result<super::uploads::UploadCommit> {
    use super::uploads::UploadCommit;
    if live_catalog(store, &session.node_id, session.generation)
        .await?
        .is_none()
    {
        return Ok(UploadCommit::Lost);
    }
    let (Some(digest), Some(edition)) = (
        session.catalog_digest.as_deref(),
        session.catalog_edition.as_deref(),
    ) else {
        return Ok(UploadCommit::Refused("unknown_title"));
    };
    // The upload wrote under the keys of the edition the holding's listing
    // sold at grant; once the listing sells another, those files are not
    // the holding's and nothing is recorded as delivered. The batch
    // re-checks this in its gate.
    if !holding_sells_edition(store, &session.book_uuid, edition).await? {
        return Ok(UploadCommit::Refused("edition_changed"));
    }
    // A key another title (or an acquire) took since the grant is not
    // recorded for this one; the batch re-checks this in its gate. Checked
    // only once the fence is known live, so a lost fence answers `Lost`.
    if !upload_holds_its_keys(store, session).await? {
        return Ok(UploadCommit::Refused("key_taken"));
    }
    let now = stamp();
    // The title still holds every key it was granted: each is recorded
    // for it with this upload as its latest writer (the keys are unique,
    // so the count says all of them). Gated in the same statement, so a
    // key taken between the check above and the batch is never recorded
    // as delivered.
    let granted_keys = granted_key_count(session);
    let statements = vec![
        exec(
            "UPDATE node_upload_sessions SET state = 'committed', committed_at = ? \
              WHERE upload_id = ? AND state = 'open' \
                AND EXISTS (SELECT 1 FROM nodes WHERE node_id = ? AND catalog_fence = ? \
                             AND share_books = 1) \
                AND EXISTS (SELECT 1 FROM node_credentials \
                             WHERE node_id = ? AND revoked_at IS NULL) \
                AND EXISTS (SELECT 1 FROM holdings WHERE id = ? AND listing_id IN \
                             (SELECT id FROM store_listings WHERE edition_id = ?)) \
                AND (SELECT COUNT(*) FROM edition_files WHERE writer = ?) = ?",
            vec![
                text(&now),
                text(&session.upload_id),
                text(&session.node_id),
                batch::int(session.generation),
                text(&session.node_id),
                text(&session.book_uuid),
                text(edition),
                text(&session.upload_id),
                batch::int(i64::try_from(granted_keys).unwrap_or(i64::MAX)),
            ],
        ),
        exec(
            "UPDATE holdings SET delivered_digest = ?, delivered_at = ?, updated_at = ? \
              WHERE id = ? AND share_digest = ? AND EXISTS (\
                SELECT 1 FROM node_upload_sessions \
                 WHERE upload_id = ? AND state = 'committed' AND committed_at = ?)",
            vec![
                text(digest),
                text(&now),
                text(&now),
                text(&session.book_uuid),
                text(digest),
                text(&session.upload_id),
                text(&now),
            ],
        ),
    ];
    let reply = batch::execute_host_batch(
        store,
        request(
            &format!("catalog-upload-commit:{}:{now}", session.upload_id),
            statements,
        ),
    )
    .await?;
    if batch::rows_affected(&reply, 0) != 1 {
        // A concurrent identical commit may have landed in between; as for
        // a job's upload, it answers committed. Otherwise the fence first,
        // as before the batch: a lost fence is `Lost`.
        let again = super::uploads::load_upload(store, &session.upload_id).await?;
        return match again {
            Some(row) if row.state == super::uploads::STATE_COMMITTED => {
                Ok(UploadCommit::Committed {
                    job_completed: false,
                })
            }
            Some(row)
                if catalog_fence_live(store, &row.node_id, row.generation).await?
                    && !holding_sells_edition(store, &row.book_uuid, edition).await? =>
            {
                Ok(UploadCommit::Refused("edition_changed"))
            }
            Some(row)
                if catalog_fence_live(store, &row.node_id, row.generation).await?
                    && !upload_holds_its_keys(store, &row).await? =>
            {
                Ok(UploadCommit::Refused("key_taken"))
            }
            _ => Ok(UploadCommit::Lost),
        };
    }
    tracing::info!(
        node_id = %session.node_id,
        fence = session.generation,
        upload_id = %session.upload_id,
        delivered = batch::rows_affected(&reply, 1) == 1,
        "committed a branch's shared title"
    );
    Ok(UploadCommit::Committed {
        job_completed: false,
    })
}

/// True while holding `holding_id`'s listing still sells `edition_id`.
///
/// # Errors
///
/// Returns the read error.
async fn holding_sells_edition(
    store: &LibraryStore,
    holding_id: &str,
    edition_id: &str,
) -> Result<bool> {
    Ok(matches!(
        store.file_owner_of_holding(holding_id).await?,
        Some(crate::store::FileOwner::Edition(owner)) if owner == edition_id
    ))
}

/// Removes every holding of `node_id` (it stopped sharing, left or was
/// revoked; `reason` is logged), each with `holding_removed`; answers how
/// many went. Files already delivered stay with their editions.
///
/// # Errors
///
/// Returns the read or write error.
pub async fn detach_branch_titles(
    store: &LibraryStore,
    node_id: &str,
    reason: &str,
) -> Result<u64> {
    detach(store, node_id, NodeHoldingSelection::All, reason).await
}

/// Removes the holdings of `node_id` that `selection` names, logging
/// `reason`; answers how many were selected.
///
/// # Errors
///
/// Returns the read or write error.
async fn detach(
    store: &LibraryStore,
    node_id: &str,
    selection: NodeHoldingSelection<'_>,
    reason: &str,
) -> Result<u64> {
    let removed = store.remove_node_holdings(node_id, selection).await?;
    if removed > 0 {
        tracing::info!(node_id, removed, reason, "removed a branch's titles");
    }
    Ok(u64::try_from(removed).unwrap_or(u64::MAX))
}

/// The condition that a file record is one `node_id`'s (or, with `None`,
/// any branch's) catalog uploads delivered for an edition nothing here holds any more: an entry left by
/// a branch that stopped sharing, withdrew the title or left. An edition
/// an account or another branch still holds keeps the records, since its
/// files are still that edition's.
fn left_behind_by(node_id: Option<&str>) -> Condition {
    let writers = match node_id {
        Some(node_id) => catalog_uploads_of(node_id),
        None => Query::select()
            .column(node_upload_sessions::Column::UploadId)
            .from(node_upload_sessions::Entity)
            .and_where(node_upload_sessions::Column::JobId.eq(CATALOG_ITEM))
            .to_owned(),
    };
    Condition::all()
        .add(edition_files::Column::OwnerKind.eq("edition"))
        .add(edition_files::Column::Writer.in_subquery(writers))
        .add(
            edition_files::Column::OwnerId.not_in_subquery(
                Query::select()
                    .column(store_listings::Column::EditionId)
                    .from(store_listings::Entity)
                    .and_where(
                        store_listings::Column::Id.in_subquery(
                            Query::select()
                                .column(holdings::Column::ListingId)
                                .from(holdings::Entity)
                                .to_owned(),
                        ),
                    )
                    .to_owned(),
            ),
        )
}

/// How many editions `node_id` (or any branch) left entries for
/// ([`left_behind_by`]), counted in the database so the answer never
/// depends on how many rows one read may return.
async fn editions_left_behind(store: &LibraryStore, node_id: Option<&str>) -> Result<u64> {
    editions::Entity::find()
        .filter(
            editions::Column::Id.in_subquery(
                Query::select()
                    .column(edition_files::Column::OwnerId)
                    .from(edition_files::Entity)
                    .cond_where(left_behind_by(node_id))
                    .to_owned(),
            ),
        )
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)
}

/// Forgets the files branches left behind (entries "from a branch that
/// left" and the like), of one branch or of every one: the
/// `edition_files` records its catalog uploads wrote for editions nothing
/// here holds any more. Answers how many editions' entries went. The files
/// themselves stay in this library's outputs.
///
/// # Errors
///
/// Returns the read or write error.
pub async fn remove_detached_branch_titles(
    store: &LibraryStore,
    node_id: Option<&str>,
) -> Result<u64> {
    let removed = editions_left_behind(store, node_id).await?;
    if removed > 0 {
        edition_files::Entity::delete_many()
            .filter(left_behind_by(node_id))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
    }
    Ok(removed)
}

/// One title a branch shares, as the owner's Settings → Branches lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchTitle {
    /// The node holding's id (a catalog upload's `book_uuid`).
    pub uuid: String,
    /// `node:<node_id>`.
    pub provenance: String,
    /// The branch's node id.
    pub node_id: String,
    /// The user it belongs to here (the branch's owner).
    pub owner_user_id: Option<i64>,
    /// Store id.
    pub store: String,
    /// Product id.
    pub product_id: String,
    /// Display title.
    pub title: String,
    /// Authors.
    pub authors: Option<String>,
    /// Storage key of the primary audio file.
    pub primary_key: String,
    /// True once the current manifest's files arrived.
    pub delivered: bool,
    /// RFC 3339 last change.
    pub updated_at: String,
}

/// Titles branches share, of one branch or all, by holding id, at most
/// `limit`.
///
/// # Errors
///
/// Returns the read error.
pub async fn list_branch_titles(
    store: &LibraryStore,
    node_id: Option<&str>,
    limit: u64,
) -> Result<Vec<BranchTitle>> {
    let mut find = find_holdings()
        .filter(holdings::Column::HolderKind.eq("node"))
        .filter(holdings::Column::ShareDigest.is_not_null())
        .order_by_asc(holdings::Column::Id);
    if let Some(node_id) = node_id {
        find = find.filter(holdings::Column::NodeId.eq(node_id));
    }
    let rows = find
        .limit(limit.min(super::trust::result_page(store)))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut owners: BTreeMap<String, Option<i64>> = BTreeMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let node = row.node_id.clone().unwrap_or_default();
        let owner = match owners.get(&node) {
            Some(owner) => *owner,
            None => {
                let owner = nodes::Entity::find_by_id(node.clone())
                    .one(store.db())
                    .await
                    .map_err(LibraryError::Orm)?
                    .and_then(|n| n.owner_user_id);
                owners.insert(node.clone(), owner);
                owner
            }
        };
        let shared: Option<CatalogTitle> = share_title_of(store, &row.id)
            .await?
            .and_then(|text| serde_json::from_str(&text).ok());
        let (store_id, product_id) = split_share_ref(row.share_ref.as_deref().unwrap_or_default());
        out.push(BranchTitle {
            delivered: row.delivered_digest.is_some() && row.delivered_digest == row.share_digest,
            uuid: row.id,
            provenance: row.holder,
            node_id: node,
            owner_user_id: owner,
            store: store_id,
            product_id,
            title: shared
                .as_ref()
                .map(|t| t.metadata.title.clone())
                .unwrap_or_default(),
            authors: shared.as_ref().and_then(|t| t.metadata.authors.clone()),
            primary_key: shared.map(|t| t.primary_key).unwrap_or_default(),
            updated_at: row.updated_at,
        });
    }
    Ok(out)
}

/// Counts of one branch's shared titles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct BranchTitleCounts {
    /// Titles it shares now.
    #[schema(example = 128)]
    pub shared: u64,
    /// Of those, titles whose files arrived.
    #[schema(example = 120)]
    pub delivered: u64,
    /// Editions whose files it delivered that nothing in this library holds
    /// any more (it stopped sharing, left, or withdrew them, and no store
    /// account or other branch here holds them).
    #[schema(example = 3)]
    pub detached: u64,
}

/// A node holding's share and delivered digests, read by column name
/// One node holding's id and share reference, read by name.
#[derive(Debug, FromQueryResult)]
struct RefRow {
    /// `holdings.id`.
    id: String,
    /// `holdings.share_ref`: the title as `store:product_id`.
    share_ref: Option<String>,
}

/// [`BranchTitleCounts`] of `node_id`.
///
/// # Errors
///
/// Returns the read error.
pub async fn branch_title_counts(store: &LibraryStore, node_id: &str) -> Result<BranchTitleCounts> {
    let shared_rows = || {
        holdings::Entity::find()
            .filter(holdings::Column::HolderKind.eq("node"))
            .filter(holdings::Column::NodeId.eq(node_id))
            .filter(holdings::Column::ShareDigest.is_not_null())
    };
    let shared = shared_rows()
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let delivered = shared_rows()
        .filter(sea_orm::sea_query::ExprTrait::equals(
            Expr::col(holdings::Column::DeliveredDigest),
            holdings::Column::ShareDigest,
        ))
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let detached = editions_left_behind(store, Some(node_id)).await?;
    Ok(BranchTitleCounts {
        shared,
        delivered,
        detached,
    })
}

/// Upper clamp for one [`forwardable_branch_title_keys_page`] read.
///
/// A key row is a handful of short columns, so this many stay far below the
/// 256 KiB inline result cap of an adapter that does not stream.
pub const FORWARDABLE_KEYS_MAX_PAGE: u64 = 256;

/// The identity of one title a branch shares with this install, without
/// its metadata or file manifest.
///
/// Read in keyset pages by [`forwardable_branch_title_keys_page`]; the full
/// title is loaded only when it is needed, by [`forwardable_branch_title`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardableBranchKey {
    /// The node holding's id; the keyset cursor of the page reader.
    pub id: String,
    /// Store id (`audible`, `libro`, ...).
    pub store: String,
    /// Store product id.
    pub product_id: String,
    /// RFC 3339 last change, as stored.
    pub updated_at: String,
    /// The files of the current manifest arrived here, so the title may be
    /// passed on. Rows without it are returned so a page's length still
    /// tells the caller whether more rows follow.
    pub delivered: bool,
}

/// One narrow node holding row as read; columns are matched by name.
///
/// Positional tuples are not used: the database proxy hands rows over as
/// maps ordered by column name, so a tuple would bind columns out of order.
#[derive(Debug, sea_orm::FromQueryResult)]
struct BranchKeyRow {
    /// Holding id.
    id: String,
    /// `store:product_id` as the branch named it.
    share_ref: Option<String>,
    /// RFC 3339 last change.
    updated_at: String,
    /// Digest of the shared manifest.
    share_digest: Option<String>,
    /// Digest of the manifest whose files were delivered.
    delivered_digest: Option<String>,
}

/// On a branch: one keyset page of the titles its own branches share with
/// it, identity columns only, by holding id.
///
/// Only keys with [`ForwardableBranchKey::delivered`] set may be passed on.
///
/// # Arguments
///
/// * `store` - This install's library.
/// * `after_id` - The last [`ForwardableBranchKey::id`] of the previous
///   page, or `None` for the first page.
/// * `limit` - Rows wanted, clamped to 1..=[`FORWARDABLE_KEYS_MAX_PAGE`].
///
/// # Returns
///
/// Up to `limit` keys in ascending id order; fewer than `limit` means the
/// last page.
///
/// # Errors
///
/// Returns the read error.
pub async fn forwardable_branch_title_keys_page(
    store: &LibraryStore,
    after_id: Option<&str>,
    limit: u64,
) -> Result<Vec<ForwardableBranchKey>> {
    let mut query = holdings::Entity::find()
        .select_only()
        .column(holdings::Column::Id)
        .column(holdings::Column::ShareRef)
        .column(holdings::Column::UpdatedAt)
        .column(holdings::Column::ShareDigest)
        .column(holdings::Column::DeliveredDigest)
        .filter(holdings::Column::HolderKind.eq("node"))
        .filter(holdings::Column::ShareDigest.is_not_null())
        .order_by_asc(holdings::Column::Id);
    if let Some(after_id) = after_id {
        query = query.filter(holdings::Column::Id.gt(after_id));
    }
    let rows = query
        .limit(limit.clamp(1, FORWARDABLE_KEYS_MAX_PAGE))
        .into_model::<BranchKeyRow>()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let (store_id, product_id) =
                split_share_ref(row.share_ref.as_deref().unwrap_or_default());
            ForwardableBranchKey {
                delivered: row.delivered_digest.is_some()
                    && row.delivered_digest == row.share_digest,
                id: row.id,
                store: store_id,
                product_id,
                updated_at: row.updated_at,
            }
        })
        .collect())
}

/// A title one of this install's branches shares, ready to pass on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardableBranchTitle {
    /// The node holding's id.
    pub id: String,
    /// The title as the branch shared it.
    pub title: CatalogTitle,
    /// RFC 3339 last change.
    pub updated_at: String,
}

/// On a branch: the title shared through node holding `id` when it may
/// still be passed on (still held, files of its current manifest
/// delivered).
///
/// # Arguments
///
/// * `store` - This install's library.
/// * `id` - The holding id, from a [`ForwardableBranchKey`].
///
/// # Returns
///
/// The title, or `None` when the holding is gone or not delivered.
///
/// # Errors
///
/// Returns the read error.
pub async fn forwardable_branch_title(
    store: &LibraryStore,
    id: &str,
) -> Result<Option<ForwardableBranchTitle>> {
    let Some(row) = find_holdings()
        .filter(holdings::Column::Id.eq(id))
        .filter(holdings::Column::HolderKind.eq("node"))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if row.share_digest.is_none() || row.delivered_digest != row.share_digest {
        return Ok(None);
    }
    Ok(share_title_of(store, &row.id)
        .await?
        .and_then(|text| serde_json::from_str::<CatalogTitle>(&text).ok())
        .map(|title| ForwardableBranchTitle {
            id: row.id,
            title,
            updated_at: row.updated_at,
        }))
}

#[cfg(test)]
#[path = "branch_catalog_tests.rs"]
mod tests;
