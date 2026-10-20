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
//! demotion that lands between claim and handoff still denies. Handoff also
//! requires the claim to still own the job's running lease, so a late
//! handoff after a reclaim, completion or expiry hands nothing over. Every input
//! is read from the database on each call; nothing is cached.
//!
//! The scan planner ([`plan_scan`]) applies the matching rule on the
//! admission side: only operator authority, an Owner or an Administrator
//! may scan every account, and that scan is split into one job per
//! scan-enabled account so each can be routed to any node trusted with
//! it. Any other user's scan covers only the accounts they own.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::enrollment::load_node_credential;
use super::instances::{default_instance_for_key, load_plugin_instance, PluginInstanceId};
use super::job_routing::{JobClaimRequest, RoutedJobClaim};
use super::semaphore::release_permits;
use crate::entities::{account_links, accounts, nodes, portal_identities, users};
use crate::error::{LibraryError, Result};
use crate::models::{BookRecord, JobKind, JobPayload, JobRecord, JobState, UserRole, UserStatus};
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

/// A store account, keyed as in `accounts`: `(plugin_key, account_id)`.
pub type AccountKey = (String, String);

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
/// A node trusted only with one user's work receives a scan or acquire
/// that names an account (`owned_job_account`) only when every store
/// account the name reaches is linked to that user.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn job_trusted(store: &LibraryStore, trust: &NodeTrust, job: &JobRecord) -> Result<bool> {
    match trust {
        NodeTrust::All => Ok(true),
        NodeTrust::Nothing => Ok(false),
        NodeTrust::OwnedBy(user_id) => match owned_job_account(job) {
            Some(account) => account_owned(store, *user_id, account).await,
            None => Ok(false),
        },
    }
}

/// The account a [`NodeTrust::OwnedBy`] node must own to receive `job`.
///
/// `None` when the job's shape alone refuses it: a kind other than scan or
/// acquire, an instance pin, requirements, or no named account.
pub(super) fn owned_job_account(job: &JobRecord) -> Option<&str> {
    if !matches!(job.kind, JobKind::Scan | JobKind::Acquire)
        || job.payload.plugin_instance_id.is_some()
        || !job.payload.requires.is_empty()
    {
        return None;
    }
    job.payload.account.as_deref()
}

/// True when `account` names at least one store account and every account
/// it names (by id or label) is linked to `user_id`.
///
/// # Errors
///
/// Returns an error when a read fails.
pub(super) async fn account_owned(
    store: &LibraryStore,
    user_id: i64,
    account: &str,
) -> Result<bool> {
    let touched = accounts_named(store, account, None).await?;
    if touched.is_empty() {
        return Ok(false);
    }
    let owned = accounts_owned_by(store, user_id).await?;
    Ok(touched.iter().all(|key| owned.contains(key)))
}

/// True when a `claimant` may run `job` at all, whatever it is trusted
/// with: a forced or PDF-only acquire runs only on a node that holds the
/// library database, since the host acquire path does neither.
#[must_use]
pub fn claimant_may_run(claimant: Claimant, job: &JobRecord) -> bool {
    claimant == Claimant::Local
        || !(job.kind == JobKind::Acquire && (job.payload.force || job.payload.pdf_only))
}

/// The store accounts a scan or acquire touches: its items.
///
/// - A scan that names an account touches every `accounts` row with that
///   id (within the pinned instance's plugin). A scan of every account is
///   `None`: what it touches is not known until it runs.
/// - An acquire touches the named account's rows, plus the account of every
///   library row its filters match (title id, account, pinned plugin),
///   whatever the row's acquire status. That is a superset of the titles it
///   will fetch, so a node must be trusted with at least everything it
///   fetches.
/// - Anything else, and a job pinned to an unknown instance, is `None`.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn job_item_accounts(
    store: &LibraryStore,
    job: &JobRecord,
) -> Result<Option<BTreeSet<AccountKey>>> {
    let plugin_key = match job.payload.plugin_instance_id.as_deref() {
        Some(raw) => {
            let Ok(id) = PluginInstanceId::parse(raw) else {
                return Ok(None);
            };
            match load_plugin_instance(store, &id).await? {
                Some(instance) => Some(instance.plugin_key),
                None => return Ok(None),
            }
        }
        None => None,
    };
    let account = job.payload.account.as_deref().filter(|a| !a.is_empty());
    let mut items = BTreeSet::new();
    if let Some(account) = account {
        items.extend(accounts_named(store, account, plugin_key.as_deref()).await?);
    }
    match job.kind {
        JobKind::Scan => Ok(account.map(|_| items)),
        JobKind::Acquire => {
            let title = job.payload.title.as_deref().filter(|t| !t.is_empty());
            for book in store.list_books(account).await? {
                if plugin_key
                    .as_deref()
                    .is_some_and(|key| key != book.plugin_key)
                    || title.is_some_and(|t| !book_matches_title(&book, t))
                {
                    continue;
                }
                items.insert((book.plugin_key, book.account_id));
            }
            Ok(Some(items))
        }
        _ => Ok(None),
    }
}

/// True when `id` names `book` (uuid, product id, ISBN or ASIN, any case),
/// as an acquire's title filter matches.
#[must_use]
pub fn book_matches_title(book: &BookRecord, id: &str) -> bool {
    id.eq_ignore_ascii_case(&book.uuid)
        || id.eq_ignore_ascii_case(&book.product_id)
        || book
            .isbn
            .as_deref()
            .is_some_and(|i| id.eq_ignore_ascii_case(i))
        || book
            .asin
            .as_deref()
            .is_some_and(|x| id.eq_ignore_ascii_case(x))
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
    if !claimant_may_run(claimant, job) {
        return Ok(false);
    }
    let trust = node_trust(store, node_id, claimant).await?;
    job_trusted(store, &trust, job).await
}

/// Whom library work a remote node asks for runs as (a `scan.request` over
/// the node API): the user who owns the node, while the node is trusted
/// with anything. `None` for a node that is trusted with nothing (revoked,
/// ownerless, a disabled owner) or unknown, which may ask for nothing.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn remote_node_requester(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Option<ScanRequester>> {
    if node_trust(store, node_id, Claimant::Remote).await? == NodeTrust::Nothing {
        return Ok(None);
    }
    Ok(nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .and_then(|row| row.owner_user_id)
        .map(ScanRequester::User))
}

/// True when a remote node may read `job`'s state.
///
/// A node trusted with everything reads every job. A node trusted only
/// with its owner's work reads any scan or acquire that names an account
/// of its owner's (`account_owned`), including a pinned one it could not
/// claim itself, since its owner may queue those through it
/// ([`plan_scan`]). A node trusted with nothing reads none.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn remote_node_may_view_job(
    store: &LibraryStore,
    node_id: &str,
    job: &JobRecord,
) -> Result<bool> {
    match node_trust(store, node_id, Claimant::Remote).await? {
        NodeTrust::All => Ok(true),
        NodeTrust::Nothing => Ok(false),
        NodeTrust::OwnedBy(user_id) => {
            let account = job.payload.account.as_deref().filter(|a| !a.is_empty());
            match account {
                Some(account) if matches!(job.kind, JobKind::Scan | JobKind::Acquire) => {
                    account_owned(store, user_id, account).await
                }
                _ => Ok(false),
            }
        }
    }
}

/// Re-checks a claimed job just before its secrets are handed to the
/// claiming node.
///
/// The claim is returned unchanged only when both hold:
///
/// - the claim still owns the job: the row is `running` under the claim's
///   fence (same lease owner and generation), that owner is
///   `request.owner`, and the lease has not expired. A claim that was
///   reclaimed, finished, released, or left to expire is not handed over;
/// - the node is still trusted with the job ([`node_may_claim_job`]).
///
/// Ownership is checked again after the trust reads, so a lease lost while
/// they ran is refused too.
///
/// Otherwise `None` is returned, so nothing is unsealed. When the claim was
/// taken by `request.owner`, the job goes back to `pending` without spending
/// an attempt if the claim's fence still owns the row (a row another fence
/// owns is left alone), and the claim's permits are released (a permit taken
/// over since holds a new token and is left alone too). A claim with another
/// lease owner is refused without touching the row or its permits.
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
        Some(job) if claim_owns_job(&claim.job, job, request.owner, Utc::now()) => {
            node_may_claim_job(store, request.node_id, request.claimant, job).await?
        }
        _ => false,
    };
    if trusted {
        // The trust reads take time; a claim that lost its lease meanwhile
        // must not be handed over on the earlier snapshot. Secrets are
        // unsealed after this returns, so the fence on every later write
        // still covers a loss after this point.
        let still = store.get_job(&claim.job.id).await?;
        if still
            .as_ref()
            .is_some_and(|job| claim_owns_job(&claim.job, job, request.owner, Utc::now()))
        {
            return Ok(Some(claim));
        }
    }
    // A claim taken under another worker's lease owner is not this
    // request's to release.
    let Some(fence) = claim.job.fence().filter(|f| f.owner == request.owner) else {
        return Ok(None);
    };
    store.release_job_claim(&fence, claim.resumed).await?;
    release_permits(store, &claim.permits).await?;
    Ok(None)
}

/// True when `claimed` (the record its claim returned) still owns `current`
/// (the row read now) for `owner` at `now`.
///
/// Compares the full fence, not only the generation: a claim record without
/// a lease owner owns nothing.
fn claim_owns_job(
    claimed: &JobRecord,
    current: &JobRecord,
    owner: &str,
    now: DateTime<Utc>,
) -> bool {
    let Some(fence) = claimed.fence() else {
        return false;
    };
    current.state == JobState::Running
        && fence.owner == owner
        && current.fence().as_ref() == Some(&fence)
        && current
            .lease_expires_at
            .is_some_and(|expires| expires > now)
}

/// `accounts` rows a job naming `needle` would touch, limited to
/// `plugin_key` when given.
///
/// Matches the way the scan executor resolves an account needle
/// (`SourceRegistry::account_needle_matches`): the account id or the label,
/// ignoring ASCII case. A narrower match here would let a Host approve a
/// job that the executor then runs against another user's account.
pub(super) async fn accounts_named(
    store: &LibraryStore,
    needle: &str,
    plugin_key: Option<&str>,
) -> Result<BTreeSet<AccountKey>> {
    let mut query = accounts::Entity::find();
    if let Some(key) = plugin_key {
        query = query.filter(accounts::Column::PluginKey.eq(key));
    }
    Ok(query
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .filter(|row| {
            row.account_id.eq_ignore_ascii_case(needle)
                || row
                    .label
                    .as_deref()
                    .is_some_and(|label| label.eq_ignore_ascii_case(needle))
        })
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
///   route each to any node trusted with that account. When the id also
///   names a scan-disabled account on another plugin, its job is pinned to
///   the default instance of each plugin where it is scan-enabled, so the
///   disabled account is never touched. With no scan-enabled
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
        let mut out = Vec::new();
        for target in scan_all_targets(store, plugin_key.as_deref(), |_| true).await? {
            if let Some(job) = target_payload(store, &target, &payload).await? {
                out.push(job);
            }
        }
        if out.is_empty() {
            return Ok(vec![payload(None)]);
        }
        return Ok(out);
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
    for target in scan_all_targets(store, plugin_key.as_deref(), wholly_owned).await? {
        if let Some(job) = target_payload(store, &target, &payload).await? {
            out.push(job);
        }
    }
    Ok(out)
}

/// Plans the acquire jobs one request admits.
///
/// `titles` are title ids (uuid, ASIN, ISBN, product id); each becomes its
/// own job, so each title can go to any node trusted with it. `account`
/// narrows to one store account and `plugin_instance_id` pins every job.
///
/// - Operator authority, an Owner or an Administrator: one job per title,
///   or one job for everything pending (it touches many accounts, and only
///   nodes trusted with every one of them claim it; splitting it per
///   account would repeat the storage listing for every account).
/// - Any other active user: a named account must be theirs, and so must
///   every library row a named title matches. A title named without an
///   account becomes one job per account id it is in, since a node trusted
///   only with its owner's work takes only jobs that name an account. With
///   neither, the request becomes one job per account they wholly own, so
///   each can go to a node trusted with it; the plan is empty when they own
///   none.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] when the user is unknown or disabled,
/// or names an account or title that is not theirs (unknown ones read the
/// same), [`LibraryError::Invalid`] for a malformed instance id, and the
/// read error when a read fails.
pub async fn plan_acquire(
    store: &LibraryStore,
    requester: ScanRequester,
    account: Option<&str>,
    titles: &[String],
    plugin_instance_id: Option<&str>,
) -> Result<Vec<JobPayload>> {
    let account = account.map(str::trim).filter(|a| !a.is_empty());
    let pinned = plugin_instance_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let mut seen = BTreeSet::new();
    let titles: Vec<&str> = titles
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty() && seen.insert(t.to_ascii_lowercase()))
        .collect();
    let owner = requester_scope(store, requester).await?;
    let payload = |account: Option<&str>, title: Option<&str>| JobPayload {
        account: account.map(str::to_string),
        title: title.map(str::to_string),
        plugin_instance_id: pinned.clone(),
        ..Default::default()
    };
    let per_title = |account: Option<&str>| -> Vec<JobPayload> {
        if titles.is_empty() {
            vec![payload(account, None)]
        } else {
            titles.iter().map(|t| payload(account, Some(t))).collect()
        }
    };
    let Some(user_id) = owner else {
        return Ok(per_title(account));
    };
    let plugin_key = match pinned.as_deref() {
        Some(raw) => {
            let id = PluginInstanceId::parse(raw)
                .map_err(|err| LibraryError::Invalid(format!("plugin instance id: {err}")))?;
            match load_plugin_instance(store, &id).await? {
                Some(instance) => Some(instance.plugin_key),
                None => return Err(not_your_account()),
            }
        }
        None => None,
    };
    let owned = accounts_owned_by(store, user_id).await?;
    let wholly_owned = |touched: &BTreeSet<AccountKey>| {
        !touched.is_empty() && touched.iter().all(|key| owned.contains(key))
    };
    if let Some(account) = account {
        if !wholly_owned(&accounts_named(store, account, plugin_key.as_deref()).await?) {
            return Err(not_your_account());
        }
    }
    if !titles.is_empty() {
        let books = store.list_books(account).await?;
        let not_yours = || {
            LibraryError::Forbidden(
                "that title is not in a store account linked to this user".into(),
            )
        };
        let mut out = Vec::new();
        for title in &titles {
            let touched: BTreeSet<AccountKey> = books
                .iter()
                .filter(|book| {
                    plugin_key
                        .as_deref()
                        .is_none_or(|key| key == book.plugin_key)
                        && book_matches_title(book, title)
                })
                .map(|book| (book.plugin_key.clone(), book.account_id.clone()))
                .collect();
            if !wholly_owned(&touched) {
                return Err(not_yours());
            }
            if account.is_some() {
                out.push(payload(account, Some(title)));
                continue;
            }
            // A Host only takes work that names an account, so the title is
            // planned once per account id it is in.
            let ids: BTreeSet<&str> = touched.iter().map(|(_, id)| id.as_str()).collect();
            for id in ids {
                if wholly_owned(&accounts_named(store, id, plugin_key.as_deref()).await?) {
                    out.push(payload(Some(id), Some(title)));
                    continue;
                }
                if plugin_key.is_some() {
                    return Err(not_yours());
                }
                // The id is shared with an account on another store that is
                // not theirs: pin one job per store of theirs that has it.
                for key in touched
                    .iter()
                    .filter(|(_, other)| other == id)
                    .map(|(key, _)| key.as_str())
                {
                    if !wholly_owned(&accounts_named(store, id, Some(key)).await?) {
                        return Err(not_yours());
                    }
                    let Some(instance) = default_instance_for_key(store, key).await? else {
                        return Err(not_yours());
                    };
                    out.push(JobPayload {
                        plugin_instance_id: Some(instance.id.to_string()),
                        ..payload(Some(id), Some(title))
                    });
                }
            }
        }
        return Ok(out);
    }
    if account.is_some() {
        return Ok(per_title(account));
    }
    let ids: BTreeSet<String> = owned
        .iter()
        .filter(|(key, _)| plugin_key.as_deref().is_none_or(|pin| pin == key))
        .map(|(_, id)| id.clone())
        .collect();
    let mut out = Vec::new();
    for id in ids {
        if wholly_owned(&accounts_named(store, &id, plugin_key.as_deref()).await?) {
            out.push(payload(Some(&id), None));
        }
    }
    Ok(out)
}

/// `None` when `requester` may act on every account (operator authority,
/// an Owner or an Administrator), otherwise the user whose accounts bound
/// the request.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] for an unknown or disabled user.
async fn requester_scope(store: &LibraryStore, requester: ScanRequester) -> Result<Option<i64>> {
    match requester {
        ScanRequester::Operator => Ok(None),
        ScanRequester::User(user_id) => match active_role(store, user_id).await? {
            Some(role) if role.is_privileged() => Ok(None),
            Some(_) => Ok(Some(user_id)),
            None => Err(LibraryError::Forbidden(
                "this user may not queue library work".into(),
            )),
        },
    }
}

/// One job a scan of every account plans.
struct ScanTarget {
    /// The account id the job names.
    account_id: String,
    /// The plugin the job must stay on, when the id also names an account
    /// with scanning turned off on another plugin.
    plugin_key: Option<String>,
    /// The accounts the job touches.
    touched: BTreeSet<AccountKey>,
}

/// Builds the payload for `target`, pinning it to its plugin's default
/// instance when it must stay on one plugin and the request pins none.
///
/// Returns `None` when that plugin has no instance to pin to.
///
/// # Errors
///
/// Returns the read error when the instance lookup fails.
async fn target_payload(
    store: &LibraryStore,
    target: &ScanTarget,
    payload: &impl Fn(Option<&str>) -> JobPayload,
) -> Result<Option<JobPayload>> {
    let mut job = payload(Some(&target.account_id));
    if let (Some(key), None) = (&target.plugin_key, &job.plugin_instance_id) {
        let Some(instance) = default_instance_for_key(store, key).await? else {
            return Ok(None);
        };
        job.plugin_instance_id = Some(instance.id.to_string());
    }
    Ok(Some(job))
}

/// The error for an account the requester does not own.
fn not_your_account() -> LibraryError {
    LibraryError::Forbidden("that store account is not linked to this user".into())
}

/// The jobs a scan of every account plans, within `plugin_key` if given.
///
/// A job names an account id, and a job's account filter matches that id or
/// label on every plugin it runs on, whatever their scan flags. So each
/// scan-enabled id is one unpinned job when every account it would touch is
/// scan-enabled, and otherwise one job per plugin where every account it
/// would touch is scan-enabled, pinned to that plugin. An account that only
/// shares its plugin with a scan-disabled account matching the same id is
/// left out, since no job can reach it alone.
///
/// Targets `allowed` refuses are dropped, and a target whose accounts the
/// kept targets already cover (an id differing only in case, or another
/// account's label) is skipped so no account is scanned twice. Targets that
/// reach more accounts are kept first, so a needle that only reaches part
/// of another's accounts is the one skipped. A chain of labels that each
/// name another account's id can still scan its middle account twice.
///
/// # Errors
///
/// Returns the read error when the account read fails.
async fn scan_all_targets(
    store: &LibraryStore,
    plugin_key: Option<&str>,
    allowed: impl Fn(&BTreeSet<AccountKey>) -> bool,
) -> Result<Vec<ScanTarget>> {
    let mut query = accounts::Entity::find();
    if let Some(key) = plugin_key {
        query = query.filter(accounts::Column::PluginKey.eq(key));
    }
    let rows = query.all(store.db()).await.map_err(LibraryError::Orm)?;
    let ids: BTreeSet<&str> = rows
        .iter()
        .filter(|row| row.scan_enabled != 0)
        .map(|row| row.account_id.as_str())
        .collect();
    let mut candidates: Vec<ScanTarget> = Vec::new();
    for id in ids {
        let matching: Vec<&accounts::Model> = rows
            .iter()
            .filter(|row| {
                row.account_id.eq_ignore_ascii_case(id)
                    || row
                        .label
                        .as_deref()
                        .is_some_and(|label| label.eq_ignore_ascii_case(id))
            })
            .collect();
        let keys = |rows: &[&accounts::Model]| -> BTreeSet<AccountKey> {
            rows.iter()
                .map(|row| (row.plugin_key.clone(), row.account_id.clone()))
                .collect()
        };
        if matching.iter().all(|row| row.scan_enabled != 0) {
            candidates.push(ScanTarget {
                account_id: id.to_string(),
                plugin_key: None,
                touched: keys(&matching),
            });
            continue;
        }
        let plugins: BTreeSet<&str> = matching
            .iter()
            .filter(|row| row.scan_enabled != 0 && row.account_id == id)
            .map(|row| row.plugin_key.as_str())
            .collect();
        for key in plugins {
            let on_plugin: Vec<&accounts::Model> = matching
                .iter()
                .copied()
                .filter(|row| row.plugin_key == key)
                .collect();
            if on_plugin.iter().all(|row| row.scan_enabled != 0) {
                candidates.push(ScanTarget {
                    account_id: id.to_string(),
                    plugin_key: Some(key.to_string()),
                    touched: keys(&on_plugin),
                });
            }
        }
    }
    // Wider targets are decided first: when another account's label equals
    // an id, the needle that reaches both is kept and the one that reaches
    // only one is then covered. Kept targets still come out in id order.
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(candidates[index].touched.len()));
    // Accounts some kept target already touches. A target whose accounts
    // are all among them (an id that differs only in case, or another
    // account's label) would scan them twice, possibly at once on two
    // nodes.
    let mut covered: BTreeSet<AccountKey> = BTreeSet::new();
    let mut keep = vec![false; candidates.len()];
    for index in order {
        let touched = &candidates[index].touched;
        // Refused targets are dropped before coalescing, so they never
        // hide a later target the requester may run.
        if !allowed(touched) || touched.is_subset(&covered) {
            continue;
        }
        covered.extend(touched.iter().cloned());
        keep[index] = true;
    }
    let out: Vec<ScanTarget> = candidates
        .into_iter()
        .zip(keep)
        .filter_map(|(target, kept)| kept.then_some(target))
        .collect();
    Ok(out)
}
