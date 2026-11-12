//! Interlibrary requests: a member of a library asks for a book one of its
//! branches listed, and the branch sends it.
//!
//! On the library:
//!
//! 1. [`plan_branch_request`] picks the branch listing for the work
//!    ([`super::branch_listings_for_works`], current answers of trusted
//!    branches only) and builds the job: a [`JobKind::BranchRequest`]
//!    pinned to that branch's node, naming the work and the listing
//!    ([`BranchRequestTarget`]). The daemon admits it, then
//!    [`notify_requests_queued`] wakes the branch. No node of this library
//!    ever runs the job (`super::claimant_may_run` refuses it).
//! 2. The branch claims the requests waiting for it
//!    ([`claim_branch_requests`], the `requests.claim` frame): each job is
//!    leased to the branch's remote lease owner, and its work goes out by
//!    identifier ([`bookclerk_node_proto::LookupWork`]), never by this
//!    library's work id alone or a title.
//! 3. [`finish_branch_request`] (the `requests.finish` frame) records what
//!    the branch did under that lease: accepted completes the job; "not
//!    listed" fails it for good; a failure on the branch fails it with the
//!    usual retries. A lease the branch lets expire goes back to pending
//!    like any job's.
//!
//! The book itself never travels with the request. Once the branch has it,
//! its catalog sync sends it like the owner's own books, and this library
//! records it as held through that branch.
//!
//! On the branch, [`record_link_request`] notes that a library asked for a
//! book, so the catalog sync sends that library the book although it is a
//! member's ([`books_requested_by_link`]).

use std::collections::BTreeSet;

use bookclerk_node_proto::{BranchRequest, LookupWork, RequestOutcome, MAX_CLAIMED_REQUESTS};
use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::{NotSet, Set},
    ColumnTrait as _, EntityTrait as _, FromQueryResult, QueryFilter as _, QueryOrder as _,
    QuerySelect as _,
};

use super::catalog_lookup::{identifiers_of, trusts};
use super::link_imports::HeldBookKey;
use super::node_queue::{notify_node, NodeMessage};
use super::remote_work::{remote_job_fence, remote_lease_owner};
use crate::entities::{jobs, link_requests};
use crate::error::{LibraryError, Result};
use crate::models::{
    branch_request_dedup_key, BookRecord, BranchRequestTarget, JobKind, JobPayload, JobState,
    JobTrigger,
};
use crate::store::LibraryStore;

/// How long a branch holds a claimed request before it goes back to
/// pending. Taking a request is a few local reads and writes on the
/// branch, so this is generous.
pub const REQUEST_LEASE_SECS: u64 = 300;

/// Pending requests read per [`claim_branch_requests`] call, so requests
/// skipped along the way (another branch's whose dedup key happens to
/// share the prefix, an unreadable payload) do not starve the batch.
const CLAIM_SCAN: u64 = 64;

/// Rows per read of [`books_requested_by_link`] when the database plugin
/// advertises no smaller `maxResultRows`.
const REQUESTED_PAGE: u64 = 256;

/// `error_kind` of a request its branch no longer lists.
pub const REQUEST_NOT_LISTED: &str = "not_listed";

/// `error_kind` of a request its branch could not take now.
pub const REQUEST_BRANCH_FAILED: &str = "branch_failed";

/// `error_kind` of a request whose work has nothing left to ask by (it was
/// merged away, or lost every identifier) when its branch came for it.
pub const REQUEST_WORK_GONE: &str = "work_gone";

/// Progress text of a request its branch accepted.
pub const REQUEST_ACCEPTED: &str = "accepted by the branch; the book arrives with its catalog sync";

/// The job a request for `work_id` becomes: which branch it goes to and
/// what it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRequestPlan {
    /// The branch node the job is pinned to.
    pub node_id: String,
    /// The job's payload ([`JobKind::BranchRequest`]).
    pub payload: JobPayload,
}

/// Plans an interlibrary request for `work_id`.
///
/// The listing comes from [`super::branch_listings_for_works`]: answers a
/// trusted branch gave within [`super::BRANCH_LISTING_TTL`]. With
/// `node_id`, only that branch's listings count; without, the first branch
/// in node order. Within a branch the first listing (store, product id)
/// is asked for; the branch picks which of its people's copies to send.
///
/// # Arguments
///
/// * `work_id` - This library's work.
/// * `node_id` - The branch to ask, or `None` for any that lists the work.
/// * `trigger` - Who asked (`api` for a member, `scheduler` for an
///   automatic request).
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] when no trusted branch (or not the
/// named one) currently lists the work, or the read error.
pub async fn plan_branch_request(
    store: &LibraryStore,
    work_id: &str,
    node_id: Option<&str>,
    trigger: JobTrigger,
) -> Result<BranchRequestPlan> {
    let listings = super::branch_listings_for_works(store, &[work_id.to_string()]).await?;
    let listing = listings
        .into_iter()
        .filter(|l| l.work_id == work_id)
        .find(|l| node_id.is_none_or(|node| l.node_id == node))
        .ok_or_else(|| {
            LibraryError::NotFound(match node_id {
                Some(node) => format!("work {work_id} through branch {node}"),
                None => format!("work {work_id} through a branch"),
            })
        })?;
    Ok(BranchRequestPlan {
        node_id: listing.node_id.clone(),
        payload: JobPayload {
            trigger,
            node_id: Some(listing.node_id),
            branch_request: Some(BranchRequestTarget {
                work_id: work_id.to_string(),
                store: listing.store,
                marketplace: listing.marketplace,
                product_id: listing.product_id,
            }),
            ..JobPayload::default()
        },
    })
}

/// Wakes branch `node_id` after a request for it was admitted. Best
/// effort: the branch also claims on every connection and on a timer.
pub async fn notify_requests_queued(store: &LibraryStore, node_id: &str) {
    notify_node(store, node_id, NodeMessage::RequestsQueued {}).await;
}

/// The node and target of a stored request job, when it is one.
fn request_of(row: &jobs::Model) -> Option<(String, BranchRequestTarget)> {
    if JobKind::parse(&row.kind) != Some(JobKind::BranchRequest) {
        return None;
    }
    let payload: JobPayload = serde_json::from_str(&row.payload).ok()?;
    Some((payload.node_id?, payload.branch_request?))
}

/// Claims the requests waiting for branch `node_id`, oldest first.
///
/// A request is waiting while its job is pending, pinned to this branch,
/// and due. Each is leased to the branch ([`remote_lease_owner`]) for
/// [`REQUEST_LEASE_SECS`]. A request whose work has no identifier left to
/// ask by is failed (`work_gone`) instead of sent.
///
/// # Returns
///
/// `None` when `node_id` is not a branch this library trusts; otherwise
/// at most [`MAX_CLAIMED_REQUESTS`] claimed requests.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn claim_branch_requests(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Option<Vec<BranchRequest>>> {
    if !trusts(store, node_id).await? {
        return Ok(None);
    }
    let prefix = branch_request_dedup_key(node_id, "");
    let now_s = Utc::now().to_rfc3339();
    let rows = jobs::Entity::find()
        .filter(jobs::Column::Kind.eq(JobKind::BranchRequest.as_str()))
        .filter(jobs::Column::State.eq(JobState::Pending.as_str()))
        // Due only, as the job store's own claims compare it, so requests
        // waiting out a retry never crowd out the ones that are due.
        .filter(jobs::Column::RunAfter.lte(now_s))
        .filter(jobs::Column::DedupKey.starts_with(prefix.as_str()))
        .order_by_desc(jobs::Column::Priority)
        .order_by_asc(jobs::Column::CreatedAt)
        .order_by_asc(jobs::Column::Id)
        .limit(CLAIM_SCAN)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let owner = remote_lease_owner(node_id);
    let candidates: Vec<(jobs::Model, BranchRequestTarget)> = rows
        .into_iter()
        .filter_map(|row| {
            let (pinned, target) = request_of(&row)?;
            (pinned == node_id).then_some((row, target))
        })
        .take(MAX_CLAIMED_REQUESTS)
        .collect();
    let works: Vec<String> = candidates
        .iter()
        .map(|(_, target)| target.work_id.clone())
        .collect();
    let mut identifiers_by_work = identifiers_of(store, &works).await?;
    let mut out = Vec::new();
    for (row, target) in candidates {
        let identifiers = identifiers_by_work
            .remove(&target.work_id)
            .unwrap_or_default();
        let request = BranchRequest {
            job: row.id.clone(),
            // The claim below moves the lease to the next generation.
            generation: row.lease_generation + 1,
            work: LookupWork {
                work: target.work_id.clone(),
                identifiers,
            },
            store: target.store,
            marketplace: target.marketplace,
            product_id: target.product_id,
        };
        if let Some(problem) = request.problem() {
            // Nothing a branch could look it up by (or a listing the frame
            // rules refuse): fail it here rather than send it.
            store
                .fail_pending_job(
                    &row.id,
                    row.lease_generation,
                    REQUEST_WORK_GONE,
                    &format!("the request cannot be sent to the branch: {problem}"),
                )
                .await?;
            continue;
        }
        let Some(claimed) = store
            .claim_job_by_id(&row.id, row.lease_generation, &owner, REQUEST_LEASE_SECS)
            .await?
        else {
            continue;
        };
        out.push(BranchRequest {
            generation: claimed.lease_generation,
            ..request
        });
    }
    Ok(Some(out))
}

/// What [`finish_branch_request`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestFinished {
    /// The outcome was recorded on the job.
    Recorded,
    /// The lease is no longer the branch's (it expired and moved on, the
    /// job was cancelled, or the job is not a request for this branch).
    Lost,
    /// The node is not a branch this library trusts.
    Refused,
}

/// Records what branch `node_id` did with the request `job_id` it claimed
/// at lease `generation`.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn finish_branch_request(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    outcome: RequestOutcome,
) -> Result<RequestFinished> {
    if !trusts(store, node_id).await? {
        return Ok(RequestFinished::Refused);
    }
    let Some(row) = jobs::Entity::find_by_id(job_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(RequestFinished::Lost);
    };
    if request_of(&row).is_none_or(|(pinned, _)| pinned != node_id) {
        return Ok(RequestFinished::Lost);
    }
    let fence = remote_job_fence(node_id, job_id, generation);
    let recorded = match outcome {
        RequestOutcome::Accepted => store.complete_job(&fence, Some(REQUEST_ACCEPTED)).await?,
        RequestOutcome::NotListed => {
            store
                .fail_job_without_retry(
                    &fence,
                    REQUEST_NOT_LISTED,
                    "the branch no longer lists this book",
                )
                .await?
        }
        RequestOutcome::Failed => {
            store
                .fail_job(
                    &fence,
                    REQUEST_BRANCH_FAILED,
                    "the branch could not take the request now",
                )
                .await?
        }
    };
    Ok(if recorded {
        RequestFinished::Recorded
    } else {
        RequestFinished::Lost
    })
}

/// On a branch: records that the library `cluster_id` asked for `book`,
/// so the catalog sync sends it there. A book already recorded for that
/// library is left as it is.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn record_link_request(
    store: &LibraryStore,
    cluster_id: &str,
    book: &BookRecord,
) -> Result<()> {
    let row = link_requests::ActiveModel {
        id: NotSet,
        cluster_id: Set(cluster_id.to_string()),
        plugin_key: Set(book.plugin_key.clone()),
        account_id: Set(book.account_id.clone()),
        marketplace: Set(bookclerk_resolve::stored_marketplace(&book.marketplace)),
        product_id: Set(bookclerk_resolve::stored_product_id(&book.product_id)),
        created_at: Set(Utc::now().to_rfc3339()),
    };
    link_requests::Entity::insert(row)
        .on_conflict(
            OnConflict::columns([
                link_requests::Column::ClusterId,
                link_requests::Column::PluginKey,
                link_requests::Column::AccountId,
                link_requests::Column::Marketplace,
                link_requests::Column::ProductId,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// One narrow `link_requests` row as read; columns are matched by name.
#[derive(Debug, FromQueryResult)]
struct RequestRow {
    /// Row id, the keyset cursor for the next page.
    id: i64,
    /// The storefront plugin that holds the requested book.
    plugin_key: String,
    /// The store account on this branch that holds it.
    account_id: String,
    /// The book's marketplace, normalized as stored (empty when none).
    marketplace: String,
    /// The book's product id, normalized as stored.
    product_id: String,
}

/// On a branch: every book the library `cluster_id` asked for, by store
/// identity. Read in keyset pages no larger than the database plugin's
/// `maxResultRows`.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn books_requested_by_link(
    store: &LibraryStore,
    cluster_id: &str,
) -> Result<BTreeSet<HeldBookKey>> {
    let page = match store.db_capabilities().max_result_rows {
        0 => REQUESTED_PAGE,
        rows => u64::from(rows).clamp(1, REQUESTED_PAGE),
    };
    let mut out = BTreeSet::new();
    let mut after: Option<i64> = None;
    loop {
        let mut query = link_requests::Entity::find()
            .select_only()
            .column(link_requests::Column::Id)
            .column(link_requests::Column::PluginKey)
            .column(link_requests::Column::AccountId)
            .column(link_requests::Column::Marketplace)
            .column(link_requests::Column::ProductId)
            .filter(link_requests::Column::ClusterId.eq(cluster_id))
            .order_by_asc(link_requests::Column::Id);
        if let Some(after) = after {
            query = query.filter(link_requests::Column::Id.gt(after));
        }
        let rows = query
            .limit(page)
            .into_model::<RequestRow>()
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= page;
        after = rows.last().map(|row| row.id);
        for row in rows {
            out.insert((
                row.plugin_key,
                row.marketplace,
                row.product_id,
                row.account_id,
            ));
        }
        if !full {
            return Ok(out);
        }
    }
}

#[cfg(test)]
#[path = "branch_requests_tests.rs"]
mod tests;
