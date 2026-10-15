//! Per-item trust: which jobs a node may claim, and the scan-all planner.
//!
//! A node that shares the library database and `master.key` (kind `owner`)
//! already holds every secret, so it may claim any job. A Host node keeps
//! its own database and receives a job's secrets from the owner, so what it
//! may claim follows the role of the user who owns it:
//!
//! - an active Owner or Administrator's Host may claim every job;
//! - any other active user's Host may claim a job only when every secret
//!   the job needs belongs to that user. For a scan or acquire that means a
//!   named store account, every `accounts` row of which (within the pinned
//!   plugin) is linked to the user through `account_links`;
//! - everything else is ineligible: a scan-all or acquire with no account,
//!   a job pinned to or requiring a plugin instance (instances carry no
//!   owner yet, so their settings belong to the operator), listen-sync,
//!   integration scans and plugin copies, a Host without an owner, a
//!   disabled or unknown owner, and a revoked or missing node credential.
//!
//! The predicate runs inside the routed claim before the compare-and-swap
//! ([`super::claim_routed_job`]) and again at handoff, just before a job's
//! secrets would be unsealed for the node ([`handoff_routed_job`]), so a
//! demotion that lands between claim and handoff still denies. Every input
//! is read from the database on each call; nothing is cached.
//!
//! The scan planner ([`plan_scan`]) applies the matching rule on the
//! admission side: only operator authority, an Owner or an Administrator
//! may scan every account, and that scan is split into one job per
//! scan-enabled account so each can be routed to any node trusted with
//! it. Any other user's scan covers only the accounts they own.

use std::collections::BTreeSet;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::enrollment::load_node_credential;
use super::instances::{load_plugin_instance, PluginInstanceId};
use super::job_routing::{JobClaimRequest, RoutedJobClaim};
use super::semaphore::release_permits;
use crate::entities::{account_links, accounts, nodes, portal_identities, users};
use crate::error::{LibraryError, Result};
use crate::models::{JobKind, JobPayload, JobRecord, UserRole, UserStatus};
use crate::store::LibraryStore;

/// How the claiming node reaches the job queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Claimant {
    /// A worker in this process, which holds the library database itself.
    /// A node id without a `nodes` row is trusted as such a worker.
    #[default]
    Local,
    /// A node that authenticated over the node API. It must have a `nodes`
    /// row and, unless it is an owner node, a live node credential.
    Remote,
}

/// What a node is trusted with, resolved from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeTrust {
    /// Every job: an owner node, or a Host owned by an active Owner or
    /// Administrator.
    All,
    /// Only jobs whose secrets all belong to this user.
    OwnedBy(i64),
    /// No job at all.
    Nothing,
}

/// Who asked for a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanRequester {
    /// Operator authority: the operator token or session, the daemon's own
    /// scheduler, or a daemon with authentication turned off.
    Operator,
    /// A signed-in user; their role is read from the database.
    User(i64),
}

/// A store account, keyed as in `accounts`.
type AccountKey = (String, String);

/// Resolves what `node_id` is trusted with right now.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn node_trust(
    store: &LibraryStore,
    node_id: &str,
    claimant: Claimant,
) -> Result<NodeTrust> {
    let row = if node_id.trim().is_empty() {
        None
    } else {
        nodes::Entity::find_by_id(node_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
    };
    let Some(row) = row else {
        return Ok(match claimant {
            Claimant::Local => NodeTrust::All,
            Claimant::Remote => NodeTrust::Nothing,
        });
    };
    // Only the exact stored kind `owner` shares the database; anything
    // else (including text this build does not know) is treated as a Host.
    if row.kind == "owner" {
        return Ok(NodeTrust::All);
    }
    let credential_live = load_node_credential(store, &row.node_id)
        .await?
        .is_some_and(|credential| credential.revoked_at.is_none());
    if !credential_live {
        return Ok(NodeTrust::Nothing);
    }
    let Some(owner) = row.owner_user_id else {
        return Ok(NodeTrust::Nothing);
    };
    Ok(match active_role(store, owner).await? {
        Some(role) if role.is_privileged() => NodeTrust::All,
        Some(_) => NodeTrust::OwnedBy(owner),
        None => NodeTrust::Nothing,
    })
}

/// The role of `user_id` when the user exists and is active.
///
/// The stored text is parsed strictly: an unknown role or status reads as
/// no user, never as a default.
async fn active_role(store: &LibraryStore, user_id: i64) -> Result<Option<UserRole>> {
    let Some(user) = users::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if UserStatus::parse(&user.status) != Some(UserStatus::Active) {
        return Ok(None);
    }
    Ok(UserRole::parse(&user.role))
}

/// True when a node with `trust` may receive `job`.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn job_trusted(store: &LibraryStore, trust: &NodeTrust, job: &JobRecord) -> Result<bool> {
    match trust {
        NodeTrust::All => Ok(true),
        NodeTrust::Nothing => Ok(false),
        NodeTrust::OwnedBy(user_id) => {
            if !matches!(job.kind, JobKind::Scan | JobKind::Acquire)
                || job.payload.plugin_instance_id.is_some()
                || !job.payload.requires.is_empty()
            {
                return Ok(false);
            }
            let Some(account) = job.payload.account.as_deref() else {
                return Ok(false);
            };
            let touched = accounts_named(store, account, None).await?;
            if touched.is_empty() {
                return Ok(false);
            }
            let owned = accounts_owned_by(store, *user_id).await?;
            Ok(touched.iter().all(|key| owned.contains(key)))
        }
    }
}

/// True when `node_id` may claim `job` right now.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn node_may_claim_job(
    store: &LibraryStore,
    node_id: &str,
    claimant: Claimant,
    job: &JobRecord,
) -> Result<bool> {
    let trust = node_trust(store, node_id, claimant).await?;
    job_trusted(store, &trust, job).await
}

/// Re-checks trust for a claimed job just before its secrets are handed to
/// the claiming node.
///
/// When the node is still trusted the claim is returned unchanged. When it
/// is not (the owner was demoted or disabled, the account was unlinked, or
/// the node was revoked since the claim), the job goes back to `pending`
/// without spending an attempt, its permits are released, and `None` is
/// returned, so nothing is unsealed.
///
/// # Errors
///
/// Returns an error when a read or write fails. A failure while releasing
/// leaves the lease to expire, after which the job is redelivered.
pub async fn handoff_routed_job(
    store: &LibraryStore,
    request: &JobClaimRequest<'_>,
    claim: RoutedJobClaim,
) -> Result<Option<RoutedJobClaim>> {
    let current = store.get_job(&claim.job.id).await?;
    let trusted = match &current {
        Some(job) => node_may_claim_job(store, request.node_id, request.claimant, job).await?,
        None => false,
    };
    if trusted {
        return Ok(Some(claim));
    }
    if let Some(fence) = claim.job.fence() {
        store.release_job_claim(&fence, claim.resumed).await?;
    }
    release_permits(store, &claim.permits).await?;
    Ok(None)
}

/// `accounts` rows with `account_id`, limited to `plugin_key` when given.
async fn accounts_named(
    store: &LibraryStore,
    account_id: &str,
    plugin_key: Option<&str>,
) -> Result<BTreeSet<AccountKey>> {
    let mut query = accounts::Entity::find().filter(accounts::Column::AccountId.eq(account_id));
    if let Some(key) = plugin_key {
        query = query.filter(accounts::Column::PluginKey.eq(key));
    }
    Ok(query
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|row| (row.plugin_key, row.account_id))
        .collect())
}

/// Store accounts linked to `user_id` through any of the user's identities.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn accounts_owned_by(store: &LibraryStore, user_id: i64) -> Result<BTreeSet<AccountKey>> {
    let identities: Vec<i64> = portal_identities::Entity::find()
        .filter(portal_identities::Column::UserId.eq(user_id))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|identity| identity.id)
        .collect();
    if identities.is_empty() {
        return Ok(BTreeSet::new());
    }
    Ok(account_links::Entity::find()
        .filter(account_links::Column::IdentityId.is_in(identities))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|link| (link.plugin_key, link.account_id))
        .collect())
}

/// Plans the scan jobs one request admits.
///
/// `account` names one store account; `plugin_instance_id` pins the scan to
/// one instance (and so to its plugin's accounts).
///
/// - Operator authority, an Owner or an Administrator: a named account is
///   one job as asked. A scan of every account becomes one job per distinct
///   scan-enabled account id (within the pinned plugin), so the planner can
///   route each to any node trusted with that account. With no scan-enabled
///   account the request stays one unsplit job, which only fully trusted
///   nodes may claim and which reports that nothing was scanned.
/// - Any other active user: a named account must be theirs, and a scan of
///   every account covers only the scan-enabled accounts they own. An
///   account id that another user also holds on a different plugin is left
///   out, since its job would touch both. The plan is empty when the user
///   owns nothing to scan.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] when the user is unknown or disabled,
/// or names an account that is not theirs (an unknown account reads the
/// same), [`LibraryError::Invalid`] for a malformed instance id, and the
/// read error when a read fails.
pub async fn plan_scan(
    store: &LibraryStore,
    requester: ScanRequester,
    account: Option<&str>,
    plugin_instance_id: Option<&str>,
) -> Result<Vec<JobPayload>> {
    let account = account.map(str::trim).filter(|a| !a.is_empty());
    let pinned = plugin_instance_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let owner = match requester {
        ScanRequester::Operator => None,
        ScanRequester::User(user_id) => match active_role(store, user_id).await? {
            Some(role) if role.is_privileged() => None,
            Some(_) => Some(user_id),
            None => {
                return Err(LibraryError::Forbidden(
                    "this user may not scan the library".into(),
                ))
            }
        },
    };
    let payload = |account: Option<&str>| JobPayload {
        account: account.map(str::to_string),
        plugin_instance_id: pinned.clone(),
        ..Default::default()
    };
    let plugin_key = match pinned.as_deref() {
        Some(raw) => {
            let id = PluginInstanceId::parse(raw)
                .map_err(|err| LibraryError::Invalid(format!("plugin instance id: {err}")))?;
            load_plugin_instance(store, &id)
                .await?
                .map(|instance| instance.plugin_key)
        }
        None => None,
    };
    let Some(user_id) = owner else {
        if let Some(account) = account {
            return Ok(vec![payload(Some(account))]);
        }
        if pinned.is_some() && plugin_key.is_none() {
            // An unknown pinned instance fails at run time, as before.
            return Ok(vec![payload(None)]);
        }
        let ids = scan_enabled_account_ids(store, plugin_key.as_deref()).await?;
        if ids.is_empty() {
            return Ok(vec![payload(None)]);
        }
        return Ok(ids.iter().map(|id| payload(Some(id))).collect());
    };
    if pinned.is_some() && plugin_key.is_none() {
        return Err(not_your_account());
    }
    let owned = accounts_owned_by(store, user_id).await?;
    let wholly_owned = |touched: &BTreeSet<AccountKey>| {
        !touched.is_empty() && touched.iter().all(|key| owned.contains(key))
    };
    if let Some(account) = account {
        let touched = accounts_named(store, account, plugin_key.as_deref()).await?;
        if !wholly_owned(&touched) {
            return Err(not_your_account());
        }
        return Ok(vec![payload(Some(account))]);
    }
    let mut out = Vec::new();
    for id in scan_enabled_account_ids(store, plugin_key.as_deref()).await? {
        let touched = accounts_named(store, &id, plugin_key.as_deref()).await?;
        if wholly_owned(&touched) {
            out.push(payload(Some(&id)));
        }
    }
    Ok(out)
}

/// The error for an account the requester does not own.
fn not_your_account() -> LibraryError {
    LibraryError::Forbidden("that store account is not linked to this user".into())
}

/// Distinct scan-enabled account ids, sorted, within `plugin_key` if given.
async fn scan_enabled_account_ids(
    store: &LibraryStore,
    plugin_key: Option<&str>,
) -> Result<BTreeSet<String>> {
    let mut query = accounts::Entity::find().filter(accounts::Column::ScanEnabled.ne(0));
    if let Some(key) = plugin_key {
        query = query.filter(accounts::Column::PluginKey.eq(key));
    }
    Ok(query
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|row| row.account_id)
        .collect())
}
