//! Branches of branches: on a library, the titles its branches share with
//! it (`catalog.*` frames; the "Branches of branches" section of the join
//! design).
//!
//! A branch is a library of its own, so joins make a tree (A ← B ← C).
//! Trust stays one hop wide: A knows B only as a host node owned by one of
//! A's users, and never learns of C. B shares the titles of its library
//! (and, unless its operator turned that off, the titles C shared with B)
//! over the node socket. Each title carries its store, product id,
//! metadata and file manifest, never an account, token or user field
//! ([`bookclerk_node_proto::CatalogTitle`]). A records it as owned by B's
//! owner user with provenance `node:<B>` in `branch_titles`, one row per
//! (provenance, store, product id).
//!
//! Every batch is checked on its own, like a host scan's results: the
//! catalog fence is the node's current one ([`begin_catalog`] bumps it),
//! the node is still a branch (kind `host`, live credential, active owner
//! user), and the branch still shares. A title A already holds for that
//! user in its own library is skipped, so nothing B got from A comes back
//! as B's. Files reuse the upload endpoint: [`grant_catalog_upload`] opens
//! an upload bound to the catalog fence (job id
//! [`bookclerk_node_proto::CATALOG_ITEM`]), and its commit records the
//! delivered manifest in one batch gated on that fence.
//!
//! When a branch stops sharing or leaves, its rows are marked detached
//! (`stopped_sharing`, `left`) and new titles stop; files already
//! delivered stay in A's outputs, and the owner may remove the entries in
//! bulk ([`remove_detached_branch_titles`]).

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_node_proto::{
    CatalogRef, CatalogTitle, UploadFile, CATALOG_ITEM, MAX_CATALOG_BATCH_TITLES,
    MAX_CATALOG_CURSOR_BYTES, MAX_CATALOG_TITLE_BYTES, MAX_CATALOG_WANTED,
};
use chrono::Utc;
use sea_orm::sea_query::{Expr, LikeExpr};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use serde::Serialize;

use super::batch::{self, exec, request, text};
use super::enrollment::load_node_credential;
use super::trust::accounts_owned_by;
use super::uploads::{open_upload_session, UploadGrant, UploadGrantRequest, UploadSession};
use crate::entities::{books, branch_titles, nodes, users};
use crate::error::{LibraryError, Result};
use crate::models::UserStatus;
use crate::store::LibraryStore;

/// `branch_titles.state` of a title its branch still shares.
pub const BRANCH_TITLE_SHARED: &str = "shared";

/// `branch_titles.state` of a title from a branch that stopped sharing,
/// left, or withdrew it.
pub const BRANCH_TITLE_DETACHED: &str = "detached";

/// Detach reason: the branch turned "Share this branch's books" off.
pub const DETACHED_STOPPED_SHARING: &str = "stopped_sharing";

/// Detach reason: the branch left the library (or was revoked).
pub const DETACHED_LEFT: &str = "left";

/// Detach reason: a full pass of the branch's catalog no longer named it.
pub const DETACHED_WITHDRAWN: &str = "withdrawn";

/// The provenance text of titles shared by `node_id`.
#[must_use]
pub fn branch_provenance(node_id: &str) -> String {
    format!("node:{node_id}")
}

/// How long a branch's catalog sync resumes incrementally before the
/// library asks for a full pass again, so titles a branch no longer has
/// (removed, or from a sub-branch that left) are withdrawn within a day.
pub const FULL_CATALOG_PASS_EVERY: chrono::Duration = chrono::Duration::hours(24);

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
/// With sharing off, every title the branch shared is marked detached
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
            pending_deliveries(store, node_id, MAX_CATALOG_WANTED).await?
        } else {
            let detached = detach_branch_titles(store, node_id, DETACHED_STOPPED_SHARING).await?;
            if detached > 0 {
                tracing::info!(
                    node_id,
                    detached,
                    "a branch stopped sharing; its titles are marked"
                );
            }
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
        /// branch's owner (no echo back).
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

/// Records one batch of `node_id`'s shared titles under `fence`, then the
/// cursor after it. Every title is checked before any is written; a title
/// this library already holds for the branch's owner is skipped. With
/// `done` (the last batch of a pass from the start), titles the pass did
/// not name are withdrawn (detached).
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
    if let Some(problem) = batch_problem(cursor, titles) {
        return Ok(CatalogAccept::Refused(problem));
    }
    let Some((row, owner)) = live_catalog(store, node_id, fence).await? else {
        return Ok(CatalogAccept::Lost);
    };
    let owned = accounts_owned_by(store, owner).await?;
    let provenance = branch_provenance(node_id);
    let now = stamp();
    let mut upserted = 0u64;
    let mut skipped = 0u64;
    let mut wanted = Vec::new();
    for title in titles {
        if held_here(store, &owned, title).await? {
            skipped += 1;
            continue;
        }
        let digest = title.manifest_digest();
        let metadata = serde_json::to_string(&title.metadata)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("catalog metadata: {err}")))?;
        let manifest = serde_json::to_string(&title.files)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("catalog manifest: {err}")))?;
        let existing = branch_titles::Entity::find()
            .filter(branch_titles::Column::Provenance.eq(&provenance))
            .filter(branch_titles::Column::Store.eq(&title.store))
            .filter(branch_titles::Column::ProductId.eq(&title.product_id))
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let delivered = match existing {
            Some(existing) => {
                branch_titles::Entity::update_many()
                    .col_expr(branch_titles::Column::OwnerUserId, Expr::value(owner))
                    .col_expr(
                        branch_titles::Column::Title,
                        Expr::value(title.metadata.title.clone()),
                    )
                    .col_expr(
                        branch_titles::Column::Authors,
                        Expr::value(title.metadata.authors.clone()),
                    )
                    .col_expr(branch_titles::Column::MetadataJson, Expr::value(metadata))
                    .col_expr(
                        branch_titles::Column::PrimaryKey,
                        Expr::value(title.primary_key.clone()),
                    )
                    .col_expr(
                        branch_titles::Column::PdfKey,
                        Expr::value(title.pdf_key.clone()),
                    )
                    .col_expr(branch_titles::Column::ManifestJson, Expr::value(manifest))
                    .col_expr(
                        branch_titles::Column::ManifestSha256,
                        Expr::value(digest.clone()),
                    )
                    .col_expr(
                        branch_titles::Column::State,
                        Expr::value(BRANCH_TITLE_SHARED),
                    )
                    .col_expr(
                        branch_titles::Column::DetachedReason,
                        Expr::value(Option::<String>::None),
                    )
                    .col_expr(
                        branch_titles::Column::DetachedAt,
                        Expr::value(Option::<String>::None),
                    )
                    .col_expr(branch_titles::Column::UpdatedAt, Expr::value(now.clone()))
                    .col_expr(branch_titles::Column::SeenAt, Expr::value(now.clone()))
                    .filter(branch_titles::Column::Id.eq(existing.id))
                    .exec(store.db())
                    .await
                    .map_err(LibraryError::Orm)?;
                existing.delivered_sha256
            }
            None => {
                let model = branch_titles::ActiveModel {
                    uuid: Set(uuid::Uuid::new_v4().hyphenated().to_string()),
                    provenance: Set(provenance.clone()),
                    node_id: Set(node_id.to_string()),
                    owner_user_id: Set(Some(owner)),
                    store: Set(title.store.clone()),
                    product_id: Set(title.product_id.clone()),
                    title: Set(title.metadata.title.clone()),
                    authors: Set(title.metadata.authors.clone()),
                    metadata_json: Set(metadata),
                    primary_key: Set(title.primary_key.clone()),
                    pdf_key: Set(title.pdf_key.clone()),
                    manifest_json: Set(manifest),
                    manifest_sha256: Set(digest.clone()),
                    delivered_sha256: Set(None),
                    state: Set(BRANCH_TITLE_SHARED.into()),
                    detached_reason: Set(None),
                    created_at: Set(now.clone()),
                    updated_at: Set(now.clone()),
                    seen_at: Set(now.clone()),
                    delivered_at: Set(None),
                    detached_at: Set(None),
                    ..Default::default()
                };
                branch_titles::Entity::insert(model)
                    .exec_without_returning(store.db())
                    .await
                    .map_err(LibraryError::Orm)?;
                None
            }
        };
        upserted += 1;
        if delivered.as_deref() != Some(digest.as_str()) && wanted.len() < MAX_CATALOG_WANTED {
            wanted.push(title.reference());
        }
    }
    // The cursor moves only under the live fence.
    let moved = nodes::Entity::update_many()
        .col_expr(
            nodes::Column::CatalogCursor,
            Expr::value(cursor.to_string()),
        )
        .col_expr(nodes::Column::CatalogSyncedAt, Expr::value(now.clone()))
        .filter(nodes::Column::NodeId.eq(node_id))
        .filter(nodes::Column::CatalogFence.eq(fence))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if moved.rows_affected != 1 {
        return Ok(CatalogAccept::Lost);
    }
    if done {
        if let Some(started) = row.catalog_pass_started_at.as_deref() {
            let withdrawn = branch_titles::Entity::update_many()
                .col_expr(
                    branch_titles::Column::State,
                    Expr::value(BRANCH_TITLE_DETACHED),
                )
                .col_expr(
                    branch_titles::Column::DetachedReason,
                    Expr::value(DETACHED_WITHDRAWN),
                )
                .col_expr(branch_titles::Column::DetachedAt, Expr::value(now.clone()))
                .filter(branch_titles::Column::NodeId.eq(node_id))
                .filter(branch_titles::Column::State.eq(BRANCH_TITLE_SHARED))
                .filter(branch_titles::Column::SeenAt.lt(started))
                .exec(store.db())
                .await
                .map_err(LibraryError::Orm)?;
            nodes::Entity::update_many()
                .col_expr(
                    nodes::Column::CatalogPassStartedAt,
                    Expr::value(Option::<String>::None),
                )
                .col_expr(nodes::Column::CatalogFullPassAt, Expr::value(now.clone()))
                .filter(nodes::Column::NodeId.eq(node_id))
                .filter(nodes::Column::CatalogFence.eq(fence))
                .exec(store.db())
                .await
                .map_err(LibraryError::Orm)?;
            if withdrawn.rows_affected > 0 {
                tracing::info!(
                    node_id,
                    withdrawn = withdrawn.rows_affected,
                    "a branch's full catalog pass withdrew titles"
                );
            }
        }
    }
    Ok(CatalogAccept::Accepted {
        upserted,
        skipped,
        wanted,
    })
}

/// Up to `limit` of `node_id`'s shared titles whose files have not
/// arrived (or changed since).
///
/// # Errors
///
/// Returns the read error.
pub async fn pending_deliveries(
    store: &LibraryStore,
    node_id: &str,
    limit: usize,
) -> Result<Vec<CatalogRef>> {
    let rows = branch_titles::Entity::find()
        .filter(branch_titles::Column::NodeId.eq(node_id))
        .filter(branch_titles::Column::State.eq(BRANCH_TITLE_SHARED))
        .order_by_asc(branch_titles::Column::Id)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows
        .into_iter()
        .filter(|row| row.delivered_sha256.as_deref() != Some(row.manifest_sha256.as_str()))
        .take(limit)
        .map(|row| CatalogRef {
            store: row.store,
            product_id: row.product_id,
        })
        .collect())
}

/// Opens an upload of one shared title's files, bound to `node_id`'s
/// catalog `fence` and received by `receiving_node_id`.
///
/// Granted only while the fence is live, for a title the branch shares,
/// when every file is copied to each of `destinations` (this node's, the
/// primary first) under its manifest key, the files are exactly the
/// recorded manifest, and no key is one another title here already uses
/// (`key_in_use`).
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
    let Some(row) = branch_titles::Entity::find()
        .filter(branch_titles::Column::Provenance.eq(branch_provenance(node_id)))
        .filter(branch_titles::Column::Store.eq(store_id))
        .filter(branch_titles::Column::ProductId.eq(product_id))
        .filter(branch_titles::Column::State.eq(BRANCH_TITLE_SHARED))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(UploadGrant::Refused("unknown_title"));
    };
    let manifest: Vec<bookclerk_node_proto::CatalogFile> =
        serde_json::from_str(&row.manifest_json).unwrap_or_default();
    let declared: BTreeMap<&str, u64> = manifest
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
    let keys: Vec<String> = declared.keys().map(|key| (*key).to_string()).collect();
    if keys_in_use(store, &row.uuid, &row.primary_key, &keys).await? {
        return Ok(UploadGrant::Refused("key_in_use"));
    }
    let request = UploadGrantRequest {
        book_uuid: &row.uuid,
        primary_key: &row.primary_key,
        pdf_key: row.pdf_key.as_deref(),
        files,
        destinations,
    };
    open_upload_session(
        store,
        node_id,
        CATALOG_ITEM,
        fence,
        receiving_node_id,
        &request,
        Some(&row.manifest_sha256),
    )
    .await
}

/// A `LIKE` pattern for keys beside `primary` sharing its stem
/// (`dir/stem.%`), wildcards in the key escaped with `\`.
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
    let mut pattern = String::with_capacity(base.len() + 2);
    for c in base.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push_str(".%");
    pattern
}

/// True when a catalog title's files would land on a key another title
/// already uses in this library: one of this library's own books (its
/// audio, its PDF, or a file beside its audio with the same stem), or
/// another branch title (any state; detached titles' files stay). A
/// branch's keys never overwrite what is already in the outputs.
async fn keys_in_use(
    store: &LibraryStore,
    uuid: &str,
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
    let titles = branch_titles::Entity::find()
        .filter(branch_titles::Column::Uuid.ne(uuid))
        .filter(
            Condition::any()
                .add(branch_titles::Column::PrimaryKey.is_in(keys.iter().cloned()))
                .add(branch_titles::Column::PdfKey.is_in(keys.iter().cloned()))
                .add(branch_titles::Column::PrimaryKey.like(LikeExpr::new(pattern).escape('\\'))),
        )
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(titles > 0)
}

/// Commits a catalog upload whose every file arrived: records the
/// delivered manifest on the title. One batch, each statement gated on the
/// upload being open, the catalog fence still being the node's and its
/// sharing on, its credential live, and the title's manifest unchanged
/// since the grant.
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
    let Some(digest) = session.catalog_digest.as_deref() else {
        return Ok(UploadCommit::Refused("unknown_title"));
    };
    let now = stamp();
    let statements = vec![
        exec(
            "UPDATE node_upload_sessions SET state = 'committed', committed_at = ? \
              WHERE upload_id = ? AND state = 'open' \
                AND EXISTS (SELECT 1 FROM nodes WHERE node_id = ? AND catalog_fence = ? \
                             AND share_books = 1) \
                AND EXISTS (SELECT 1 FROM node_credentials \
                             WHERE node_id = ? AND revoked_at IS NULL)",
            vec![
                text(&now),
                text(&session.upload_id),
                text(&session.node_id),
                batch::int(session.generation),
                text(&session.node_id),
            ],
        ),
        exec(
            "UPDATE branch_titles SET delivered_sha256 = ?, delivered_at = ?, updated_at = ? \
              WHERE uuid = ? AND manifest_sha256 = ? AND EXISTS (\
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
        return Ok(UploadCommit::Lost);
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

/// Marks every title `node_id` shares detached with `reason`; answers how
/// many changed. Files already delivered stay where they are.
///
/// # Errors
///
/// Returns the write error.
pub async fn detach_branch_titles(
    store: &LibraryStore,
    node_id: &str,
    reason: &str,
) -> Result<u64> {
    let updated = branch_titles::Entity::update_many()
        .col_expr(
            branch_titles::Column::State,
            Expr::value(BRANCH_TITLE_DETACHED),
        )
        .col_expr(
            branch_titles::Column::DetachedReason,
            Expr::value(reason.to_string()),
        )
        .col_expr(branch_titles::Column::DetachedAt, Expr::value(stamp()))
        .filter(branch_titles::Column::NodeId.eq(node_id))
        .filter(branch_titles::Column::State.eq(BRANCH_TITLE_SHARED))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(updated.rows_affected)
}

/// Removes detached entries ("from a branch that left" and the like), of
/// one branch or of every one; answers how many. Only the library entries
/// go: files already delivered stay in this library's outputs.
///
/// # Errors
///
/// Returns the write error.
pub async fn remove_detached_branch_titles(
    store: &LibraryStore,
    node_id: Option<&str>,
) -> Result<u64> {
    let mut delete = branch_titles::Entity::delete_many()
        .filter(branch_titles::Column::State.eq(BRANCH_TITLE_DETACHED));
    if let Some(node_id) = node_id {
        delete = delete.filter(branch_titles::Column::NodeId.eq(node_id));
    }
    Ok(delete
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .rows_affected)
}

/// One title a branch shared, as the owner's Settings → Branches lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchTitle {
    /// Stable id.
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
    /// `shared` or `detached`.
    pub state: String,
    /// `stopped_sharing`, `left` or `withdrawn`.
    pub detached_reason: Option<String>,
    /// RFC 3339 last change.
    pub updated_at: String,
}

impl From<branch_titles::Model> for BranchTitle {
    fn from(row: branch_titles::Model) -> Self {
        Self {
            delivered: row.delivered_sha256.as_deref() == Some(row.manifest_sha256.as_str()),
            uuid: row.uuid,
            provenance: row.provenance,
            node_id: row.node_id,
            owner_user_id: row.owner_user_id,
            store: row.store,
            product_id: row.product_id,
            title: row.title,
            authors: row.authors,
            primary_key: row.primary_key,
            state: row.state,
            detached_reason: row.detached_reason,
            updated_at: row.updated_at,
        }
    }
}

/// Titles branches shared, of one branch or all, oldest first, at most
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
    let mut find = branch_titles::Entity::find().order_by_asc(branch_titles::Column::Id);
    if let Some(node_id) = node_id {
        find = find.filter(branch_titles::Column::NodeId.eq(node_id));
    }
    Ok(find
        .limit(limit)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(BranchTitle::from)
        .collect())
}

/// Counts of one branch's shared titles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BranchTitleCounts {
    /// Titles it shares now.
    pub shared: u64,
    /// Of those, titles whose files arrived.
    pub delivered: u64,
    /// Titles marked detached (it stopped sharing, left, or withdrew them).
    pub detached: u64,
}

/// [`BranchTitleCounts`] of `node_id`.
///
/// # Errors
///
/// Returns the read error.
pub async fn branch_title_counts(store: &LibraryStore, node_id: &str) -> Result<BranchTitleCounts> {
    let count = |state: &'static str| {
        branch_titles::Entity::find()
            .filter(branch_titles::Column::NodeId.eq(node_id))
            .filter(branch_titles::Column::State.eq(state))
            .count(store.db())
    };
    let shared = count(BRANCH_TITLE_SHARED)
        .await
        .map_err(LibraryError::Orm)?;
    let detached = count(BRANCH_TITLE_DETACHED)
        .await
        .map_err(LibraryError::Orm)?;
    let delivered = branch_titles::Entity::find()
        .select_only()
        .column(branch_titles::Column::ManifestSha256)
        .column(branch_titles::Column::DeliveredSha256)
        .filter(branch_titles::Column::NodeId.eq(node_id))
        .filter(branch_titles::Column::State.eq(BRANCH_TITLE_SHARED))
        .filter(branch_titles::Column::DeliveredSha256.is_not_null())
        .into_tuple::<(String, Option<String>)>()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .filter(|(manifest, delivered)| delivered.as_deref() == Some(manifest.as_str()))
        .count() as u64;
    Ok(BranchTitleCounts {
        shared,
        delivered,
        detached,
    })
}

/// On a branch: the titles its own branches shared with it that it may
/// pass on (still shared, files delivered), oldest first.
///
/// # Errors
///
/// Returns the read error.
pub async fn forwardable_branch_titles(store: &LibraryStore) -> Result<Vec<branch_titles::Model>> {
    Ok(branch_titles::Entity::find()
        .filter(branch_titles::Column::State.eq(BRANCH_TITLE_SHARED))
        .order_by_asc(branch_titles::Column::Id)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .filter(|row| row.delivered_sha256.as_deref() == Some(row.manifest_sha256.as_str()))
        .collect())
}

#[cfg(test)]
#[path = "branch_catalog_tests.rs"]
mod tests;
