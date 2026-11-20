//! Linking a signed-in store account to a portal identity, and revoking
//! that link, as atomic batches serialized per store account.
//!
//! A store login stores the account's secrets (the plugin writes them
//! while it signs in) before the portal links the account. Three writers
//! can meet on one `(plugin_key, account_id)` between those two steps:
//!
//! - an allowed login linking the account ([`link_signed_in_account`]);
//! - a refused login taking back what it stored
//!   ([`super::revoke_unlinked_account`]), and a person disconnecting the
//!   account everywhere ([`super::withdraw_person_account`]);
//! - the linked person revoking the account ([`revoke_account_link`]).
//!
//! Each of them is one batch that first takes the account's
//! `bookclerk_slots` write lock ([`account_slot`]), so on every database
//! profile (`READ COMMITTED` PostgreSQL included) their guards read what
//! the previous batch committed. The guards keep one invariant: a link
//! exists only while the account's sign-in is stored, and a refused or
//! revoked account keeps no secrets. A login whose sign-in was taken back
//! before it linked is told so ([`SignInLink::SignInTakenBack`]) rather
//! than linking an account with nothing to sign in with.

use bookclerk_plugin_abi::{DbValue, TypedDbStatement};
use chrono::Utc;

use super::batch::{self, int, text};
use crate::error::Result;
use crate::secrets::secret_account_type;
use crate::store::LibraryStore;

/// The `bookclerk_slots` key that serializes writes to one store account's
/// sign-in and link.
///
/// # Arguments
///
/// * `plugin_key` - The storefront's PluginKey.
/// * `account_id` - The store account id.
#[must_use]
pub fn account_slot(plugin_key: &str, account_id: &str) -> String {
    // Length-prefixed so no pair of ids can spell another pair's key.
    format!(
        "store-account:{}:{plugin_key}:{account_id}",
        plugin_key.len()
    )
}

/// Statements that take the account's write lock for the rest of the
/// batch: insert the slot row when missing, then bump it.
///
/// The row is never deleted: a batch waiting on the bump must find the
/// row the holder bumped, or two later batches could run unserialized.
pub(super) fn lock_account_statements(plugin_key: &str, account_id: &str) -> Vec<TypedDbStatement> {
    let slot = account_slot(plugin_key, account_id);
    vec![
        batch::exec(
            "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) \
             SELECT ?, 0 WHERE NOT EXISTS (\
                SELECT 1 FROM bookclerk_slots WHERE slot_key = ?\
             )",
            vec![text(&slot), text(&slot)],
        ),
        batch::exec(
            "UPDATE bookclerk_slots SET bump = bump + 1 WHERE slot_key = ?",
            vec![text(&slot)],
        ),
    ]
}

/// SQL condition: the account's store sign-in (its integration secrets)
/// is stored. Binds: account type, plugin key, account id.
const SIGNED_IN: &str = "EXISTS (SELECT 1 FROM encrypted_secrets \
                         WHERE account_type = ? AND provider = ? AND account_id = ?)";

/// Binds for [`SIGNED_IN`].
fn signed_in_binds(plugin_key: &str, account_id: &str) -> Vec<DbValue> {
    vec![
        text(secret_account_type::INTEGRATION),
        text(plugin_key),
        text(account_id),
    ]
}

/// What [`link_signed_in_account`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInLink {
    /// The account is now linked to the identity, active with scans on.
    Linked,
    /// The identity already held the link; the account is active again
    /// when its sign-in is stored.
    AlreadyYours,
    /// Another identity holds the link; nothing changed.
    LinkedToSomeoneElse,
    /// The account's sign-in was taken back (a refused login of the same
    /// store account, or a revoke, ran after this login stored it), so
    /// nothing was linked. Signing in again connects it.
    SignInTakenBack,
}

/// Finishes an allowed store login: marks the account active with scans
/// on and links it to `identity_id`, in one atomic batch under the
/// account's write lock, only while the account's sign-in is stored and
/// no other identity holds it.
///
/// # Arguments
///
/// * `store` - The library.
/// * `identity_id` - The portal identity the login ran for.
/// * `plugin_key` - The storefront's PluginKey.
/// * `account_id` - The store account the login signed in to.
/// * `source` - The storefront alias recorded on the link (display only).
///
/// # Returns
///
/// What happened; see [`SignInLink`].
///
/// # Errors
///
/// Returns the write error.
pub async fn link_signed_in_account(
    store: &LibraryStore,
    identity_id: i64,
    plugin_key: &str,
    account_id: &str,
    source: &str,
) -> Result<SignInLink> {
    let now = Utc::now().to_rfc3339();
    let mut statements = lock_account_statements(plugin_key, account_id);
    let mut activate = vec![
        text(&now),
        text(plugin_key),
        text(account_id),
        text(plugin_key),
        text(account_id),
        int(identity_id),
    ];
    activate.extend(signed_in_binds(plugin_key, account_id));
    statements.push(batch::exec(
        &format!(
            "UPDATE accounts SET connection_status = 'active', scan_enabled = 1, updated_at = ? \
             WHERE plugin_key = ? AND account_id = ? \
             AND NOT EXISTS (SELECT 1 FROM account_links WHERE plugin_key = ? \
             AND account_id = ? AND identity_id <> ?) AND {SIGNED_IN}"
        ),
        activate,
    ));
    let link_index = statements.len();
    let mut link = vec![
        int(identity_id),
        text(plugin_key),
        text(account_id),
        text(source),
        text(&now),
        text(plugin_key),
        text(account_id),
        text(plugin_key),
        text(account_id),
    ];
    link.extend(signed_in_binds(plugin_key, account_id));
    statements.push(batch::exec(
        &format!(
            "INSERT OR IGNORE INTO account_links \
             (identity_id, plugin_key, account_id, source, created_at) \
             SELECT ?, ?, ?, ?, ? \
             WHERE EXISTS (SELECT 1 FROM accounts WHERE plugin_key = ? AND account_id = ?) \
             AND NOT EXISTS (SELECT 1 FROM account_links WHERE plugin_key = ? AND account_id = ?) \
             AND {SIGNED_IN}"
        ),
        link,
    ));
    let holder_index = statements.len();
    statements.push(batch::query(
        "SELECT identity_id FROM account_links WHERE plugin_key = ? AND account_id = ?",
        vec![text(plugin_key), text(account_id)],
        1,
    ));
    let operation_id = format!("link-sign-in-{}", uuid::Uuid::new_v4());
    let reply = store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    if batch::rows_affected(&reply, link_index) == 1 {
        return Ok(SignInLink::Linked);
    }
    let holder = batch::rows(&reply, holder_index)
        .first()
        .and_then(|row| row.values.first())
        .and_then(|cell| match cell {
            DbValue::Int64(id) => Some(*id),
            _ => None,
        });
    Ok(match holder {
        Some(id) if id == identity_id => SignInLink::AlreadyYours,
        Some(_) => SignInLink::LinkedToSomeoneElse,
        None => SignInLink::SignInTakenBack,
    })
}

/// What [`revoke_account_link`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRevoked {
    /// The account was marked revoked, its sign-in deleted and the
    /// identity's link removed.
    Revoked,
    /// The identity held no link to the account when the batch ran (a
    /// concurrent revoke finished first); nothing changed.
    NotLinked,
}

/// Revokes the account `identity_id` holds: in one atomic batch under the
/// account's write lock, marks it `revoked` with scans off, deletes its
/// integration secrets, forgets which branch kept its login for this
/// library (its `node:` shares and their sign-in revisions), and removes
/// the link. Every statement applies only while the identity's link
/// exists, and the link goes last, so the batch refuses as a whole once
/// the link is gone.
///
/// The best-effort store sign-out, and the removal of the account's
/// holdings and scan checkpoints ([`LibraryStore::revoke_credentials`]),
/// run before it while the link still exists, so a failure there can be
/// retried.
///
/// # Arguments
///
/// * `store` - The library.
/// * `identity_id` - The portal identity revoking its link.
/// * `plugin_key` - The storefront's PluginKey.
/// * `account_id` - The store account id.
///
/// # Returns
///
/// What happened; see [`LinkRevoked`].
///
/// # Errors
///
/// Returns the write error. The batch may have committed when the reply
/// was lost; retrying then answers [`LinkRevoked::NotLinked`].
pub async fn revoke_account_link(
    store: &LibraryStore,
    identity_id: i64,
    plugin_key: &str,
    account_id: &str,
) -> Result<LinkRevoked> {
    const LINKED: &str = "EXISTS (SELECT 1 FROM account_links \
                          WHERE identity_id = ? AND plugin_key = ? AND account_id = ?)";
    let linked = || vec![int(identity_id), text(plugin_key), text(account_id)];
    let with_linked = |mut binds: Vec<DbValue>| {
        binds.extend(linked());
        binds
    };
    let now = Utc::now().to_rfc3339();
    let forget_keeper = |table: &str| {
        batch::exec(
            &format!(
                "DELETE FROM {table} \
                 WHERE plugin_key = ? AND account_id = ? AND peer LIKE ? AND {LINKED}"
            ),
            with_linked(vec![text(plugin_key), text(account_id), text("node:%")]),
        )
    };
    let mut statements = lock_account_statements(plugin_key, account_id);
    statements.extend([
        batch::exec(
            &format!(
                "UPDATE accounts SET scan_enabled = 0, connection_status = 'revoked', \
                 updated_at = ? WHERE plugin_key = ? AND account_id = ? AND {LINKED}"
            ),
            with_linked(vec![text(&now), text(plugin_key), text(account_id)]),
        ),
        batch::exec(
            &format!(
                "DELETE FROM encrypted_secrets \
                 WHERE account_type = ? AND provider = ? AND account_id = ? AND {LINKED}"
            ),
            with_linked(signed_in_binds(plugin_key, account_id)),
        ),
        forget_keeper("account_credential_revisions"),
        forget_keeper("account_shares"),
    ]);
    let unlink_index = statements.len();
    statements.push(batch::exec(
        "DELETE FROM account_links WHERE identity_id = ? AND plugin_key = ? AND account_id = ?",
        linked(),
    ));
    let operation_id = format!("revoke-link-{}", uuid::Uuid::new_v4());
    let committed = store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await;
    // Purged even when the reply was lost: the batch may have committed.
    crate::secrets::cache_invalidate_account(account_id);
    Ok(if batch::rows_affected(&committed?, unlink_index) == 1 {
        LinkRevoked::Revoked
    } else {
        LinkRevoked::NotLinked
    })
}
