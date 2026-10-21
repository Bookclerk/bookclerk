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

use std::collections::BTreeSet;

use super::job_routing::job_source_plugin_key;
use super::remote_work::{fenced_job, remote_job_fence};
use super::trust::{
    accounts_named, accounts_owned_by, node_may_claim_job, node_trust, Claimant, NodeTrust,
};
use crate::error::Result;
use crate::models::JobKind;
use crate::store::{LibraryStore, NewBook};

/// Most rows one results batch may carry.
pub const MAX_SCAN_RESULT_ROWS: usize = 200;

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

/// Validates one batch of a Host's scan rows and writes it to the library.
///
/// `books` are the rows as the guest produced them; their `uuid`,
/// `plugin_key` and `source` are ignored and set here (`source` from the
/// account row), so a Host cannot address an existing row by id or label
/// its rows as another plugin's.
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
    books: Vec<NewBook>,
) -> Result<ScanAccept> {
    if books.len() > MAX_SCAN_RESULT_ROWS {
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
        NodeTrust::OwnedBy(user_id) => Some(accounts_owned_by(store, user_id).await?),
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
    for book in &books {
        if book.product_id.trim().is_empty() || book.account_id.trim().is_empty() {
            return Ok(ScanAccept::Refused("invalid_row"));
        }
        if job_accounts.as_ref().is_some_and(|accounts| {
            !accounts.contains(&(plugin_key.to_string(), book.account_id.clone()))
        }) {
            return Ok(ScanAccept::Refused("account_not_in_job"));
        }
        if let Some(owned) = &owned {
            if !owned.contains(&(plugin_key.to_string(), book.account_id.clone())) {
                return Ok(ScanAccept::Refused("not_your_account"));
            }
        }
        if seen_accounts.insert(book.account_id.clone()) {
            let Some(account) = store.get_account(plugin_key, &book.account_id).await? else {
                return Ok(ScanAccept::Refused("unknown_account"));
            };
            sources.insert(book.account_id.clone(), account.source);
        }
    }
    let mut upserted = 0u64;
    for mut book in books {
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
            .upsert_book(&book)
            .await?;
        upserted += 1;
    }
    Ok(ScanAccept::Accepted { upserted })
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
        NodeTrust::OwnedBy(user_id) => Some(Some(accounts_owned_by(store, user_id).await?)),
        NodeTrust::Nothing => None,
    })
}
