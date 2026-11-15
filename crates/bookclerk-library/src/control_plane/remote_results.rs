//! Scan results a Host node returns over the node socket (H-4 decision 6).
//!
//! A Host never writes library SQL. A scan it runs sends its book rows in
//! fenced batches; [`accept_scan_results`] validates each batch and writes
//! it through the library's own store API ([`crate::SourceScope`]), so the
//! rows land exactly as a local scan's would. Every batch is checked on its
//! own, immediately before its rows are written:
//!
//! - the fence is the job's live lease for this node, and the node is still
//!   trusted with the job ([`node_may_claim_job`]), so a demotion, an
//!   unlinked account or a revoked credential stops the next batch;
//! - the job is a scan, and the rows' plugin is the job's source plugin
//!   ([`job_source_plugin_key`]), never a destination it also requires;
//! - every row names a store account that exists under that plugin, is the
//!   job's account when the job names one, and, for a Host trusted only
//!   with its owner's accounts, is one of that user's accounts.
//!
//! A batch with any row that fails is refused whole; nothing of it is
//! written. While a batch is written, the fence and the node's trust
//! (with its owner's accounts) are confirmed again before each row, so a
//! lease lost, a node revoked, an owner disabled or an account unlinked
//! mid-batch stops the rows not yet written (answered as lost). The rows
//! already written stay, as they would for a local scan stopped there.
//!
//! After its rows, a Host reports how each account's pass ended, as a local
//! pass would record it:
//!
//! - [`accept_scan_removed`] takes product ids a delta pass reported gone
//!   from one account, in batches of at most
//!   `bookclerk_node_proto::MAX_SCAN_REMOVED_IDS`, and removes those
//!   holdings;
//! - [`accept_scan_pass_end`] records the pass's sync token (the owner hands
//!   it back with the account's next Host scan, and its own scans use it
//!   too) and, after a complete full pass, removes the account's holdings
//!   that no row of this job listed.
//!
//! Every row a Host returns stamps the holding it lists with a pass id
//! derived from the job and fence ([`handed_pass_id`]), so a pass the owner
//! handed again under a new fence never counts the earlier attempt's rows.
//! Both reports pass the same checks as a row naming that account, at the
//! moment they are applied.

use std::collections::BTreeSet;

use super::job_routing::job_source_plugin_key;
use super::remote_work::{fenced_job, remote_job_fence};
use super::trust::{
    accounts_named, accounts_owned_by, node_may_claim_job, node_trust, Claimant, NodeTrust,
};
use crate::error::Result;
use crate::models::JobKind;
use crate::store::{LibraryStore, ScannedTitle};

/// Most rows one results batch may carry.
pub const MAX_SCAN_RESULT_ROWS: usize = 200;

/// The pass id a Host's scan of `job_id` under lease generation
/// `generation` stamps on the holdings its rows list
/// (`holdings.seen_pass`).
#[must_use]
pub fn handed_pass_id(job_id: &str, generation: i64) -> String {
    format!("host:{generation}:{job_id}")
}

/// Outcome of [`accept_scan_results`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanAccept {
    /// The batch was written.
    Accepted {
        /// Rows written.
        upserted: u64,
    },
    /// The fence no longer owns the job, or the node is no longer trusted
    /// with it; nothing was written.
    Lost,
    /// The batch is not allowed; nothing was written (the reason is
    /// stable).
    Refused(&'static str),
}

/// A product a Host's pass listed that has no library row to write (its
/// record maps onto no flat row, an ebook for instance), so the owner only
/// marks the account's holding of it seen by the pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSighting {
    /// The account the pass listed it for.
    pub account_id: String,
    /// The storefront product id it listed.
    pub product_id: String,
}

/// Validates one batch of a Host's scan rows and writes it to the library.
///
/// The same as [`accept_scan_rows`] with no sightings beyond the rows.
///
/// # Errors
///
/// Returns an error when a read or a write fails.
pub async fn accept_scan_results(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    plugin_key: &str,
    books: Vec<impl Into<ScannedTitle>>,
) -> Result<ScanAccept> {
    accept_scan_rows(
        store,
        node_id,
        job_id,
        generation,
        plugin_key,
        books,
        Vec::new(),
    )
    .await
}

/// Validates one batch of a Host's scan rows and writes it to the library,
/// marking every listed product seen by the Host's pass.
///
/// `books` are the rows as the guest produced them; their `uuid`,
/// `plugin_key` and `source` are ignored and set here (`source` from the
/// account row), so a Host cannot address an existing row by id or label
/// its rows as another plugin's. `sightings` are products the same batch
/// listed that map onto no row; they pass the same account checks and are
/// only marked seen, so a complete full pass's prune keeps them.
///
/// # Errors
///
/// Returns an error when a read or a write fails.
pub async fn accept_scan_rows(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    plugin_key: &str,
    books: Vec<impl Into<ScannedTitle>>,
    sightings: Vec<ScanSighting>,
) -> Result<ScanAccept> {
    let books: Vec<ScannedTitle> = books.into_iter().map(Into::into).collect();
    if books.len().saturating_add(sightings.len()) > MAX_SCAN_RESULT_ROWS {
        return Ok(ScanAccept::Refused("batch_too_large"));
    }
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(ScanAccept::Lost);
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(ScanAccept::Lost);
    }
    if job.kind != JobKind::Scan {
        return Ok(ScanAccept::Refused("not_a_scan"));
    }
    // Only the job's source plugin scans; a destination or integration
    // plugin the job also requires never produces library rows.
    if job_source_plugin_key(store, &job).await?.as_deref() != Some(plugin_key) {
        return Ok(ScanAccept::Refused("plugin_not_in_job"));
    }
    let owned = match node_trust(store, node_id, Claimant::Remote).await? {
        NodeTrust::All => None,
        NodeTrust::OwnedBy(user_id) | NodeTrust::AllOfOwner(user_id) => {
            Some(accounts_owned_by(store, user_id).await?)
        }
        NodeTrust::Nothing => return Ok(ScanAccept::Lost),
    };
    // The job's selector resolved as the claim resolved it (id or label,
    // ignoring case), so the canonical id a host was handed validates.
    let job_accounts = match job.payload.account.as_deref().filter(|a| !a.is_empty()) {
        Some(selector) => Some(accounts_named(store, selector, Some(plugin_key)).await?),
        None => None,
    };
    // Every row is checked before any is written.
    let mut sources = std::collections::BTreeMap::new();
    let mut seen_accounts = BTreeSet::new();
    let named = books
        .iter()
        .map(|t| (&t.book.account_id, &t.book.product_id))
        .chain(sightings.iter().map(|s| (&s.account_id, &s.product_id)));
    for (account_id, product_id) in named {
        if product_id.trim().is_empty() || account_id.trim().is_empty() {
            return Ok(ScanAccept::Refused("invalid_row"));
        }
        if job_accounts.as_ref().is_some_and(|accounts| {
            !accounts.contains(&(plugin_key.to_string(), account_id.clone()))
        }) {
            return Ok(ScanAccept::Refused("account_not_in_job"));
        }
        if let Some(owned) = &owned {
            if !owned.contains(&(plugin_key.to_string(), account_id.clone())) {
                return Ok(ScanAccept::Refused("not_your_account"));
            }
        }
        if seen_accounts.insert(account_id.clone()) {
            let Some(account) = store.get_account(plugin_key, account_id).await? else {
                return Ok(ScanAccept::Refused("unknown_account"));
            };
            sources.insert(account_id.clone(), account.source);
        }
    }
    let pass_id = handed_pass_id(job_id, generation);
    let mut listed: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut upserted = 0u64;
    for mut title in books {
        let book = &mut title.book;
        // Authority is confirmed again right before each row is written, so
        // a lease lost or trust withdrawn mid-batch stops the rest of it.
        match still_authorized(store, node_id, &fence).await? {
            None => return Ok(ScanAccept::Lost),
            Some(Some(owned))
                if !owned.contains(&(plugin_key.to_string(), book.account_id.clone())) =>
            {
                return Ok(ScanAccept::Lost);
            }
            Some(_) => {}
        }
        let source = sources.get(&book.account_id).cloned().unwrap_or_default();
        book.uuid = None;
        store
            .scope(plugin_key, source.as_str())
            .upsert_scanned_title(&title)
            .await?;
        listed
            .entry(title.book.account_id.clone())
            .or_default()
            .push(title.book.product_id.clone());
        upserted += 1;
    }
    if !sightings.is_empty() {
        match still_authorized(store, node_id, &fence).await? {
            None => return Ok(ScanAccept::Lost),
            Some(Some(owned))
                if sightings
                    .iter()
                    .any(|s| !owned.contains(&(plugin_key.to_string(), s.account_id.clone()))) =>
            {
                return Ok(ScanAccept::Lost);
            }
            Some(_) => {}
        }
        for sighting in sightings {
            listed
                .entry(sighting.account_id)
                .or_default()
                .push(sighting.product_id);
        }
    }
    // Every listed product counts as still held by this pass, so a complete
    // full pass's end leaves it in place.
    for (account_id, product_ids) in listed {
        store
            .mark_holdings_seen(plugin_key, &account_id, &pass_id, &product_ids)
            .await?;
    }
    Ok(ScanAccept::Accepted { upserted })
}

/// Checks that the node may report on `account_id` of the scan job at
/// `generation`, as [`accept_scan_results`] checks a row naming it.
///
/// # Returns
///
/// `None` when it may; otherwise the answer to give.
///
/// # Errors
///
/// Returns an error when a read fails.
async fn account_report_refusal(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    plugin_key: &str,
    account_id: &str,
) -> Result<Option<ScanAccept>> {
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(Some(ScanAccept::Lost));
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(Some(ScanAccept::Lost));
    }
    if job.kind != JobKind::Scan {
        return Ok(Some(ScanAccept::Refused("not_a_scan")));
    }
    if job_source_plugin_key(store, &job).await?.as_deref() != Some(plugin_key) {
        return Ok(Some(ScanAccept::Refused("plugin_not_in_job")));
    }
    if account_id.trim().is_empty() {
        return Ok(Some(ScanAccept::Refused("invalid_row")));
    }
    let key = (plugin_key.to_string(), account_id.to_string());
    if let Some(selector) = job.payload.account.as_deref().filter(|a| !a.is_empty()) {
        if !accounts_named(store, selector, Some(plugin_key))
            .await?
            .contains(&key)
        {
            return Ok(Some(ScanAccept::Refused("account_not_in_job")));
        }
    }
    match node_trust(store, node_id, Claimant::Remote).await? {
        NodeTrust::All => {}
        NodeTrust::OwnedBy(user_id) | NodeTrust::AllOfOwner(user_id) => {
            if !accounts_owned_by(store, user_id).await?.contains(&key) {
                return Ok(Some(ScanAccept::Refused("not_your_account")));
            }
        }
        NodeTrust::Nothing => return Ok(Some(ScanAccept::Lost)),
    }
    if store.get_account(plugin_key, account_id).await?.is_none() {
        return Ok(Some(ScanAccept::Refused("unknown_account")));
    }
    Ok(None)
}

/// Removes the holdings a Host's delta pass reported gone from one account.
///
/// # Arguments
///
/// * `store` - The owner's library.
/// * `node_id` - The Host.
/// * `job_id` - The scan job.
/// * `generation` - The lease generation the Host holds.
/// * `plugin_key` - The job's source plugin.
/// * `account_id` - The account the ids left.
/// * `product_ids` - The ids, as `bookclerk_node_proto::scan_removed_problem`
///   accepts them.
///
/// # Returns
///
/// [`ScanAccept::Accepted`] with the holdings removed in `upserted`, or why
/// nothing was removed.
///
/// # Errors
///
/// Returns an error when a read or a write fails.
pub async fn accept_scan_removed(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    plugin_key: &str,
    account_id: &str,
    product_ids: &[String],
) -> Result<ScanAccept> {
    if let Some(problem) = bookclerk_node_proto::scan_removed_problem(account_id, product_ids) {
        return Ok(ScanAccept::Refused(problem));
    }
    if let Some(refusal) =
        account_report_refusal(store, node_id, job_id, generation, plugin_key, account_id).await?
    {
        return Ok(refusal);
    }
    let removed = store
        .remove_account_holdings(
            plugin_key,
            account_id,
            crate::HoldingSelection::Products(product_ids),
        )
        .await?;
    Ok(ScanAccept::Accepted {
        upserted: u64::try_from(removed).unwrap_or(u64::MAX),
    })
}

/// How a Host's pass of one account ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandedPassEnd<'a> {
    /// The account the pass scanned.
    pub account_id: &'a str,
    /// The token the plugin returned for the account's next pass; empty
    /// when it keeps none.
    pub sync_token: &'a str,
    /// The pass listed every title the account holds.
    pub complete: bool,
    /// The pass was full (sent no sync token), so its rows are everything
    /// the account holds when it is also complete.
    pub full: bool,
}

/// Records how a Host's pass of one account ended.
///
/// A complete full pass listed every title the account holds, so the
/// account's holdings no row of this job and fence listed are removed
/// first. Then the sync token is stored and, for a complete pass, the
/// time of the last complete pass, as a local pass's end records them.
///
/// # Returns
///
/// [`ScanAccept::Accepted`] with the holdings removed in `upserted`, or why
/// nothing was recorded.
///
/// # Errors
///
/// Returns an error when a read or a write fails.
pub async fn accept_scan_pass_end(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    plugin_key: &str,
    end: HandedPassEnd<'_>,
) -> Result<ScanAccept> {
    if let Some(problem) =
        bookclerk_node_proto::scan_pass_end_problem(end.account_id, end.sync_token)
    {
        return Ok(ScanAccept::Refused(problem));
    }
    if let Some(refusal) = account_report_refusal(
        store,
        node_id,
        job_id,
        generation,
        plugin_key,
        end.account_id,
    )
    .await?
    {
        return Ok(refusal);
    }
    let mut removed = 0;
    if end.complete && end.full {
        let pass_id = handed_pass_id(job_id, generation);
        removed = store
            .remove_account_holdings(
                plugin_key,
                end.account_id,
                crate::HoldingSelection::UnseenBy(&pass_id),
            )
            .await?;
    }
    store
        .finish_scan_pass(plugin_key, end.account_id, end.sync_token, end.complete)
        .await?;
    Ok(ScanAccept::Accepted {
        upserted: u64::try_from(removed).unwrap_or(u64::MAX),
    })
}

/// The sync token the owner hands with a Host scan of one account: the
/// stored token while the account's last complete pass is recent enough to
/// build on, otherwise empty (a full pass), as a local pass would start.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn handed_sync_token(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
) -> Result<String> {
    // A pass the owner stopped part way is not resumed on a Host: the
    // token is the last finished pass's, the one a fresh pass would send.
    let now = chrono::Utc::now();
    Ok(store
        .scan_state(plugin_key, account_id)
        .await?
        .filter(|state| {
            state
                .last_complete_at
                .is_some_and(|at| now - at <= crate::FULL_PASS_INTERVAL)
        })
        .map(|state| state.sync_token)
        .unwrap_or_default())
}

/// The node's authority over the job at `fence` now: `None` when the
/// fence is no longer the job's live lease for the node or the node is no
/// longer trusted with the job; otherwise the store accounts it may write
/// rows for (`None` inside meaning every account).
///
/// # Errors
///
/// Returns an error when a read fails.
async fn still_authorized(
    store: &LibraryStore,
    node_id: &str,
    fence: &crate::models::JobFence,
) -> Result<Option<Option<BTreeSet<(String, String)>>>> {
    let Some(job) = fenced_job(store, fence).await? else {
        return Ok(None);
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(None);
    }
    Ok(match node_trust(store, node_id, Claimant::Remote).await? {
        NodeTrust::All => Some(None),
        NodeTrust::OwnedBy(user_id) | NodeTrust::AllOfOwner(user_id) => {
            Some(Some(accounts_owned_by(store, user_id).await?))
        }
        NodeTrust::Nothing => None,
    })
}
