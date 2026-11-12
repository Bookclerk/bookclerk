//! Store connections a branch adds to, or withdraws from, this library over
//! the node socket ("Accounts on every instance" in the library catalog
//! network ADR).
//!
//! | Frame | Answer |
//! |---|---|
//! | `accounts.add {sealed}` | `accounts.added {added, already_here, taken, failed, not_allowed}` or `error {code: "refused"}` |
//! | `accounts.remove {plugin_key, account_id}` | `ok` or `error {code: "refused"}` |
//!
//! The branch is this connection's authenticated node, and the person is
//! the user its node row names as owner; nothing in a frame names another
//! node or user. An offer is an account bundle sealed to this connection's
//! owner box key and bound to the cluster and the node, so it opens only
//! here and only for this branch. Only accounts of storefronts every
//! installed copy declares `credentials = "shareable"` are kept, and this
//! library's per-member rule decides whether the person may connect any.
//!
//! Both frames run one at a time on this owner node ([`ACCOUNTS_GATE`]),
//! and each is refused unless the connection it came on is still the
//! branch's live one. A branch that timed out waiting for an offer and
//! then disconnected the account over a new connection therefore never
//! has the stale offer land after the removal: either the offer finished
//! first and the removal takes it back, or the offer finds its connection
//! superseded.

use bookclerk_library::control_plane::{
    import_offered_accounts, load_cluster_row, load_node, withdraw_person_account,
};
use bookclerk_library::{LibraryStore, UserStatus};
use bookclerk_node_proto::{
    open_account_offer, Body, CarriedAccount, Frame, MAX_CARRIED_ACCOUNT_ID_BYTES,
    MAX_CARRIED_PLUGIN_KEY_BYTES,
};
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

/// `accounts.add`: opens the offer, keeps its shareable accounts for the
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
    let shareable = super::branch::shareable_plugin_keys(&host.state, library).await;
    let (keep, refused): (Vec<CarriedAccount>, Vec<CarriedAccount>) = bundle
        .accounts
        .into_iter()
        .partition(|account| shareable.contains(&account.plugin_key));
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

/// A report count as the frame's `u64`.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
