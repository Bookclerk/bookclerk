//! Store connections a branch adds to, or withdraws from, this library over
//! the node socket ("Accounts on every instance" in the library catalog
//! network ADR).
//!
//! | Frame | Answer |
//! |---|---|
//! | `accounts.add {sealed}` | `accounts.added {added, already_here, taken, failed, not_allowed}` or `error {code: "refused"}` |
//! | `accounts.remove {plugin_key, account_id}` | `ok` or `error {code: "refused"}` |
//! | `accounts.update {sealed, revision}` | `accounts.updated {outcome}` or `error {code: "refused"}` |
//!
//! The branch is this connection's authenticated node, and the person is
//! the user its node row names as owner; nothing in a frame names another
//! node or user. An offer is an account bundle sealed to this connection's
//! owner box key and bound to the cluster and the node, so it opens only
//! here and only for this branch. An account of a storefront every installed
//! copy declares `credentials = "shareable"` is kept; so is a single-keeper
//! one unless another branch already keeps it here (one keeper per
//! account), and this library's per-member rule decides whether the person
//! may connect any.
//!
//! `accounts.update` is the branch keeping a single-keeper account's login
//! fresh for the person ([`bookclerk_library::control_plane::apply_keeper_update`]):
//! it replaces this library's copy only for an account of a storefront
//! that is not shareable, linked to the branch's owner, shared with this
//! branch, and only above the last revision applied from it.
//!
//! Both frames run one at a time on this owner node ([`ACCOUNTS_GATE`]),
//! and each is refused unless the connection it came on is still the
//! branch's live one. A branch that timed out waiting for an offer and
//! then disconnected the account over a new connection therefore never
//! has the stale offer land after the removal: either the offer finished
//! first and the removal takes it back, or the offer finds its connection
//! superseded.

use bookclerk_library::control_plane::{
    apply_keeper_update, import_offered_accounts, keeping_branch, load_cluster_row, load_node,
    withdraw_person_account, KeeperUpdate,
};
use bookclerk_library::{LibraryStore, UserStatus};
use bookclerk_node_proto::{
    open_account_offer, Body, CarriedAccount, Frame, KeeperOutcome, MAX_CARRIED_ACCOUNT_ID_BYTES,
    MAX_CARRIED_PLUGIN_KEY_BYTES,
};
use bookclerk_plugin_host::CredentialSharing;
use serde_json::json;

use super::work::{error, HostContext};

/// Serializes `accounts.add` and `accounts.remove` on this owner node,
/// across every branch connection. Both are rare, person-driven actions.
static ACCOUNTS_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The branch's owner user, while one is assigned and active.
async fn branch_owner(
    library: &LibraryStore,
    host: &HostContext,
) -> bookclerk_library::Result<Option<i64>> {
    let node = load_node(library, &host.node_id).await?;
    let Some(user_id) = node.owner_user_id else {
        return Ok(None);
    };
    Ok(library
        .get_user(user_id)
        .await?
        .filter(|user| user.status == UserStatus::Active)
        .map(|user| user.id))
}

/// `accounts.add`: opens the offer, keeps the accounts that may come for the
/// branch's owner under this library's rule, and answers what was kept.
///
/// # Errors
///
/// Returns a read error from before any account is written.
pub(super) async fn add(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    sealed: &str,
) -> bookclerk_library::Result<Frame> {
    let _serialized = ACCOUNTS_GATE.lock().await;
    // An offer from a connection that is no longer the branch's live one
    // (replaced, even by one that has since ended too) may be one the
    // branch gave up on before disconnecting the account over another.
    if !super::ws::is_live_session(&host.node_id, host.serial) {
        return Ok(error(id, "refused", "superseded"));
    }
    let Some(user_id) = branch_owner(library, host).await? else {
        return Ok(error(id, "refused", "no_owner"));
    };
    let Some(cluster) = load_cluster_row(library).await? else {
        return Ok(error(id, "refused", "no_cluster"));
    };
    let Ok(bundle) = open_account_offer(
        &host.owner_box_secret,
        &cluster.cluster_id,
        &host.node_id,
        sealed,
    ) else {
        tracing::warn!(node_id = %host.node_id, "a branch's store connections do not open for this connection");
        return Ok(error(id, "refused", "sealed_value_invalid"));
    };
    // The secrets are new to this process: scrub them from anything it
    // prints from here on, whether or not they are kept.
    for account in &bundle.accounts {
        for secret in &account.secrets {
            bookclerk_config::register_secret_bytes(secret.value.expose());
        }
    }
    let sharing = super::branch::credential_sharing(&host.state, library).await;
    let mut keep = Vec::new();
    let mut refused = Vec::new();
    for account in bundle.accounts {
        // A storefront not installed here is refused; a single-keeper
        // account is kept only when no other branch keeps it already.
        let kept = match sharing.get(&account.plugin_key) {
            Some(CredentialSharing::Shareable) => true,
            Some(CredentialSharing::SingleKeeper) => kept_by(library, &account)
                .await?
                .is_none_or(|node| node == host.node_id),
            None => false,
        };
        if kept {
            keep.push(account);
        } else {
            refused.push(account);
        }
    }
    let report = import_offered_accounts(library, user_id, &host.node_id, &keep).await?;
    let detail = json!({
        "node_id": host.node_id,
        "offered": keep.len() + refused.len(),
        "added": report.imported.len(),
        // Accounts already here whose share with this branch went
        // unrecorded: a disconnect everywhere here would miss the branch.
        "unrecorded": report.unrecorded.len(),
        "not_allowed": report.refused_by_rule,
    })
    .to_string();
    if let Err(err) = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "branch_connections_added",
            Some(&detail),
        )
        .await
    {
        tracing::warn!(node_id = %host.node_id, error = %err, "could not audit a branch's added store connections");
    }
    Ok(Frame::new(
        id,
        Body::AccountsAdded {
            added: count(report.imported.len()),
            already_here: count(report.already_here.len()),
            taken: count(report.taken.len()),
            failed: count(report.failed.len() + refused.len()),
            not_allowed: report.refused_by_rule,
        },
    ))
}

/// `accounts.remove`: the branch's person disconnected this store account
/// everywhere; their links to it here go, and the account with them when
/// no other link holds it.
///
/// # Errors
///
/// Returns a read or write error.
pub(super) async fn remove(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    plugin_key: &str,
    account_id: &str,
) -> bookclerk_library::Result<Frame> {
    if plugin_key.is_empty()
        || plugin_key.len() > MAX_CARRIED_PLUGIN_KEY_BYTES
        || account_id.is_empty()
        || account_id.len() > MAX_CARRIED_ACCOUNT_ID_BYTES
    {
        return Ok(error(id, "refused", "invalid_account"));
    }
    let _serialized = ACCOUNTS_GATE.lock().await;
    // Likewise a removal left on an old connection: the branch may have
    // added the account again over the live one since.
    if !super::ws::is_live_session(&host.node_id, host.serial) {
        return Ok(error(id, "refused", "superseded"));
    }
    let Some(user_id) = branch_owner(library, host).await? else {
        return Ok(error(id, "refused", "no_owner"));
    };
    let outcome = withdraw_person_account(library, user_id, plugin_key, account_id).await?;
    let detail = json!({
        "node_id": host.node_id,
        "plugin_key": plugin_key,
        "outcome": format!("{outcome:?}"),
    })
    .to_string();
    if let Err(err) = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "branch_connection_removed",
            Some(&detail),
        )
        .await
    {
        tracing::warn!(node_id = %host.node_id, error = %err, "could not audit a branch's removed store connection");
    }
    Ok(Frame::new(id, Body::Accepted {}))
}

/// `accounts.update`: the branch's newest sign-in of one single-keeper
/// account it keeps for its owner; applied when newer than the last one
/// applied from it.
///
/// # Errors
///
/// Returns a read or write error.
pub(super) async fn update(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    sealed: &str,
    revision: i64,
) -> bookclerk_library::Result<Frame> {
    if revision <= 0 {
        return Ok(error(id, "refused", "invalid_revision"));
    }
    let _serialized = ACCOUNTS_GATE.lock().await;
    if !super::ws::is_live_session(&host.node_id, host.serial) {
        return Ok(error(id, "refused", "superseded"));
    }
    let Some(user_id) = branch_owner(library, host).await? else {
        return Ok(error(id, "refused", "no_owner"));
    };
    let Some(cluster) = load_cluster_row(library).await? else {
        return Ok(error(id, "refused", "no_cluster"));
    };
    let Ok(bundle) = open_account_offer(
        &host.owner_box_secret,
        &cluster.cluster_id,
        &host.node_id,
        sealed,
    ) else {
        tracing::warn!(node_id = %host.node_id, "a branch's pushed sign-in does not open for this connection");
        return Ok(error(id, "refused", "sealed_value_invalid"));
    };
    for account in &bundle.accounts {
        for secret in &account.secrets {
            bookclerk_config::register_secret_bytes(secret.value.expose());
        }
    }
    let [account] = bundle.accounts.as_slice() else {
        return Ok(error(id, "refused", "one_account"));
    };
    // A shareable storefront's copies each refresh on their own; only a
    // single keeper's are kept in step, and only one installed here.
    let sharing = super::branch::credential_sharing(&host.state, library).await;
    // Only the branch that keeps the account here pushes its sign-ins.
    let keeper = kept_by(library, account).await?;
    let outcome = if sharing.get(&account.plugin_key) != Some(&CredentialSharing::SingleKeeper)
        || keeper.as_deref() != Some(host.node_id.as_str())
    {
        KeeperOutcome::NotShared
    } else {
        match apply_keeper_update(library, user_id, &host.node_id, account, revision).await {
            Ok(KeeperUpdate::Applied) => KeeperOutcome::Applied,
            Ok(KeeperUpdate::Stale) => KeeperOutcome::Stale,
            Ok(KeeperUpdate::NotShared) => KeeperOutcome::NotShared,
            Err(bookclerk_library::LibraryError::Invalid(detail)) => {
                return Ok(error(id, "refused", &detail));
            }
            Err(err) => return Err(err),
        }
    };
    if outcome == KeeperOutcome::Applied {
        let detail = json!({
            "node_id": host.node_id,
            "plugin_key": account.plugin_key,
        })
        .to_string();
        if let Err(err) = library
            .insert_security_audit_event(
                &format!("user:{user_id}"),
                "branch_sign_in_applied",
                Some(&detail),
            )
            .await
        {
            tracing::warn!(node_id = %host.node_id, error = %err, "could not audit a branch's pushed sign-in");
        }
    }
    Ok(Frame::new(id, Body::AccountsUpdated { outcome }))
}

/// The branch that keeps `account` for this library, if one does.
async fn kept_by(
    library: &LibraryStore,
    account: &CarriedAccount,
) -> bookclerk_library::Result<Option<String>> {
    keeping_branch(library, &account.plugin_key, &account.account_id).await
}

/// A report count as the frame's `u64`.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
