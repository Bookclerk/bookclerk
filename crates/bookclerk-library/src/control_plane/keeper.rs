//! One refresh keeper for each single-keeper store account that travelled
//! with its person ("Accounts" in `docs/adr/library-catalog-network.md`).
//!
//! A storefront that declares `credentials = "single_keeper"` (Audible)
//! must not have one login refreshed in several places. When such an
//! account is shared between a library and the person's branch, the branch
//! keeps it: its sign-ins and refreshes are the account's, and it pushes
//! each new sign-in up the person's links (`accounts.update`). The library
//! never saves its own refreshes of it, applies only the branch's pushes,
//! and only ones newer than the last it applied
//! (`account_credential_revisions`).
//!
//! Which side keeps follows from `account_shares`, so no lease row is
//! needed: a library holds a share with the branch node (`node:<id>`), the
//! branch holds one with the library (`link:<cluster>`). An account with no
//! share stays its own keeper. Which storefronts are single-keeper is the
//! daemon's to decide from the plugins' manifests; these functions take the
//! account as given.

use std::collections::BTreeMap;

use bookclerk_node_proto::CarriedAccount;
use bookclerk_plugin_abi::DbValue;
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::batch::{self, int, text};
use super::carried_accounts::node_peer;
use crate::entities::{account_credential_revisions, account_shares, encrypted_secrets};
use crate::error::{LibraryError, Result};
use crate::secrets::{build_sealed_record, secret_account_type};
use crate::store::LibraryStore;

/// Prefix of an `account_shares.peer` that names a branch node.
const NODE_PEER: &str = "node:";

/// Prefix of an `account_shares.peer` that names a library this install
/// joined.
const LINK_PEER: &str = "link:";

/// The keeper's revision of an account's sign-in: milliseconds since the
/// Unix epoch of its newest secret write here, or `None` when it has no
/// secret.
///
/// # Errors
///
/// Returns the read error, or [`LibraryError::Invalid`] for a stored time
/// that does not parse.
pub async fn sign_in_revision(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
) -> Result<Option<i64>> {
    let rows = encrypted_secrets::Entity::find()
        .filter(encrypted_secrets::Column::AccountType.eq(secret_account_type::INTEGRATION))
        .filter(encrypted_secrets::Column::Provider.eq(plugin_key))
        .filter(encrypted_secrets::Column::AccountId.eq(account_id))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut newest = None;
    for row in rows {
        let at = chrono::DateTime::parse_from_rfc3339(&row.updated_at)
            .map_err(|err| LibraryError::Invalid(format!("secret updated_at: {err}")))?
            .timestamp_millis();
        newest = Some(newest.map_or(at, |seen: i64| seen.max(at)));
    }
    Ok(newest)
}

/// The branch that keeps this account for this library: the node of its
/// earliest-recorded share with a branch, or `None` when no branch holds
/// it (this library keeps it).
///
/// # Errors
///
/// Returns the read error.
pub async fn keeping_branch(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
) -> Result<Option<String>> {
    let mut shares = account_shares::Entity::find()
        .filter(account_shares::Column::PluginKey.eq(plugin_key))
        .filter(account_shares::Column::AccountId.eq(account_id))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    shares.sort_by(|a, b| (&a.shared_at, &a.peer).cmp(&(&b.shared_at, &b.peer)));
    Ok(shares
        .into_iter()
        .find_map(|share| share.peer.strip_prefix(NODE_PEER).map(str::to_owned)))
}

/// True when this install keeps the account for the library it joined as
/// `cluster_id` (it holds a share with that library).
///
/// # Errors
///
/// Returns the read error.
pub async fn keeps_for_link(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
    cluster_id: &str,
) -> Result<bool> {
    let peer = format!("{LINK_PEER}{cluster_id}");
    Ok(account_shares::Entity::find()
        .filter(account_shares::Column::PluginKey.eq(plugin_key))
        .filter(account_shares::Column::AccountId.eq(account_id))
        .filter(account_shares::Column::Peer.eq(peer))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some())
}

/// One account this install shares with a library it joined, as the keeper
/// sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptShare {
    /// Provenance-qualified plugin key of the storefront.
    pub plugin_key: String,
    /// Store account id.
    pub account_id: String,
    /// The library's cluster id.
    pub cluster_id: String,
    /// The revision last pushed to that library; 0 before the first push.
    pub pushed: i64,
}

/// Every account this install shares with a library it joined, each library
/// once, with the revision last pushed there.
///
/// # Errors
///
/// Returns the read error.
pub async fn kept_shares(store: &LibraryStore) -> Result<Vec<KeptShare>> {
    let shares = account_shares::Entity::find()
        .filter(account_shares::Column::Peer.starts_with(LINK_PEER))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let pushed: BTreeMap<(String, String, String), i64> =
        account_credential_revisions::Entity::find()
            .filter(account_credential_revisions::Column::Peer.starts_with(LINK_PEER))
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|row| ((row.plugin_key, row.account_id, row.peer), row.revision))
            .collect();
    let mut kept: BTreeMap<(String, String, String), KeptShare> = BTreeMap::new();
    for share in shares {
        let Some(cluster_id) = share.peer.strip_prefix(LINK_PEER) else {
            continue;
        };
        let key = (share.plugin_key, share.account_id, share.peer.clone());
        let revision = pushed.get(&key).copied().unwrap_or(0);
        kept.entry(key.clone()).or_insert_with(|| KeptShare {
            plugin_key: key.0,
            account_id: key.1,
            cluster_id: cluster_id.to_owned(),
            pushed: revision,
        });
    }
    Ok(kept.into_values().collect())
}

/// Records that the keeper pushed `revision` of the account to the library
/// `cluster_id` (whether it applied it or already had it). Never lowers the
/// recorded revision.
///
/// # Errors
///
/// Returns the write error.
pub async fn record_pushed_revision(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
    cluster_id: &str,
    revision: i64,
) -> Result<()> {
    let peer = format!("{LINK_PEER}{cluster_id}");
    let now = Utc::now().to_rfc3339();
    let statements = revision_statements(plugin_key, account_id, &peer, revision, &now, None);
    let operation_id = format!("keeper-pushed-{}", uuid::Uuid::new_v4());
    store
        .execute_host_batch(batch::request(&operation_id, statements.to_vec()))
        .await?;
    Ok(())
}

/// The two statements that raise one share's revision to `revision`
/// (update, then insert), each applied only while `guard` holds and the
/// stored revision is lower.
fn revision_statements(
    plugin_key: &str,
    account_id: &str,
    peer: &str,
    revision: i64,
    now: &str,
    guard: Option<(&str, Vec<DbValue>)>,
) -> [bookclerk_plugin_abi::TypedDbStatement; 2] {
    let (guard_sql, guard_binds) = guard.map_or((String::new(), Vec::new()), |(sql, binds)| {
        (format!(" AND {sql}"), binds)
    });
    let mut update = vec![
        int(revision),
        text(now),
        text(plugin_key),
        text(account_id),
        text(peer),
        int(revision),
    ];
    update.extend(guard_binds.iter().cloned());
    let mut insert = vec![
        text(plugin_key),
        text(account_id),
        text(peer),
        int(revision),
        text(now),
        text(plugin_key),
        text(account_id),
    ];
    insert.extend(guard_binds);
    [
        batch::exec(
            &format!(
                "UPDATE account_credential_revisions SET revision = ?, updated_at = ? \
                 WHERE plugin_key = ? AND account_id = ? AND peer = ? AND revision < ?{guard_sql}"
            ),
            update,
        ),
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO account_credential_revisions \
                 (plugin_key, account_id, peer, revision, updated_at) \
                 SELECT ?, ?, ?, ?, ? WHERE EXISTS ( \
                    SELECT 1 FROM accounts WHERE plugin_key = ? AND account_id = ?){guard_sql}"
            ),
            insert,
        ),
    ]
}

/// What [`apply_keeper_update`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeeperUpdate {
    /// The account's secrets here now hold the pushed sign-in.
    Applied,
    /// This revision, or a newer one, was already applied from the branch.
    Stale,
    /// The account is not `user_id`'s here, or not shared with the branch.
    NotShared,
}

/// Applies a keeper branch's push of one account's newest sign-in.
///
/// One atomic batch replaces the account's secrets (re-sealed with this
/// install's key) and records `revision`, every statement applied only
/// while the account is linked to `user_id` (the branch's owner), shared
/// with the branch `node_id`, and `revision` is above the last one applied
/// from it. So a push that lost a race with a newer one, or that arrives
/// after the person disconnected the account, changes nothing.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a revision that is not positive or
/// an account without secrets, a seal error, or a read or write error.
pub async fn apply_keeper_update(
    store: &LibraryStore,
    user_id: i64,
    node_id: &str,
    account: &CarriedAccount,
    revision: i64,
) -> Result<KeeperUpdate> {
    if revision <= 0 {
        return Err(LibraryError::Invalid(
            "keeper revision must be positive".into(),
        ));
    }
    if account.secrets.is_empty() {
        return Err(LibraryError::Invalid(
            "a pushed sign-in carries no secret".into(),
        ));
    }
    let peer = node_peer(node_id);
    let guard = "plugin_key = ? AND account_id = ? \
                 AND EXISTS (SELECT 1 FROM account_links WHERE plugin_key = ? AND account_id = ? \
                    AND identity_id IN (SELECT id FROM portal_identities WHERE user_id = ?)) \
                 AND EXISTS (SELECT 1 FROM account_shares WHERE plugin_key = ? AND account_id = ? \
                    AND peer = ?) \
                 AND ? > COALESCE((SELECT revision FROM account_credential_revisions \
                    WHERE plugin_key = ? AND account_id = ? AND peer = ?), 0)";
    let guard_binds = || {
        vec![
            text(&account.plugin_key),
            text(&account.account_id),
            text(&account.plugin_key),
            text(&account.account_id),
            int(user_id),
            text(&account.plugin_key),
            text(&account.account_id),
            text(&peer),
            int(revision),
            text(&account.plugin_key),
            text(&account.account_id),
            text(&peer),
        ]
    };
    // Each statement runs its guard against `accounts`, so the account's
    // own row names it.
    let held = format!("EXISTS (SELECT 1 FROM accounts WHERE {guard})");
    let now = Utc::now().to_rfc3339();
    let mut statements = Vec::new();
    let mut records = Vec::new();
    for secret in &account.secrets {
        let record = build_sealed_record(
            secret.value.expose(),
            &secret.kind,
            &account.plugin_key,
            secret_account_type::INTEGRATION,
            &account.account_id,
            &secret.name,
        )?;
        let sealed = vec![
            text(&record.format),
            batch::blob(&record.ciphertext),
            batch::text_opt(record.cipher_algorithm.as_deref()),
            batch::blob_opt(record.cipher_nonce.as_deref()),
        ];
        let names = vec![
            text(&record.kind),
            text(&account.plugin_key),
            text(secret_account_type::INTEGRATION),
            text(&account.account_id),
            text(&record.name),
        ];
        let mut update = sealed.clone();
        update.push(text(&now));
        update.extend(names.clone());
        update.extend(guard_binds());
        statements.push(batch::exec(
            &format!(
                "UPDATE encrypted_secrets SET format = ?, ciphertext = ?, cipher_algorithm = ?, \
                 cipher_nonce = ?, kdf_algorithm = NULL, kdf_salt = NULL, kdf_m_cost = NULL, \
                 kdf_t_cost = NULL, kdf_p_cost = NULL, updated_at = ? \
                 WHERE kind = ? AND provider = ? AND account_type = ? AND account_id = ? \
                 AND name = ? AND {held}"
            ),
            update,
        ));
        let mut insert = names;
        insert.extend(sealed);
        insert.push(text(&now));
        insert.push(text(&now));
        insert.extend(guard_binds());
        statements.push(batch::exec(
            &format!(
                "INSERT OR IGNORE INTO encrypted_secrets \
                 (kind, provider, account_type, account_id, name, format, ciphertext, \
                  cipher_algorithm, cipher_nonce, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE {held}"
            ),
            insert,
        ));
        records.push(record);
    }
    // The revision goes last: once it is raised, the guard no longer holds.
    let revision_index = statements.len();
    statements.extend(revision_statements(
        &account.plugin_key,
        &account.account_id,
        &peer,
        revision,
        &now,
        Some((&held, guard_binds())),
    ));
    let operation_id = format!("keeper-update-{}", uuid::Uuid::new_v4());
    let reply = store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    for record in &records {
        crate::secrets::forget_cached_secret(record);
    }
    let raised = batch::rows_affected(&reply, revision_index)
        + batch::rows_affected(&reply, revision_index + 1);
    if raised > 0 {
        return Ok(KeeperUpdate::Applied);
    }
    let applied = account_credential_revisions::Entity::find()
        .filter(account_credential_revisions::Column::PluginKey.eq(&account.plugin_key))
        .filter(account_credential_revisions::Column::AccountId.eq(&account.account_id))
        .filter(account_credential_revisions::Column::Peer.eq(&peer))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some_and(|row| row.revision >= revision);
    Ok(if applied {
        KeeperUpdate::Stale
    } else {
        KeeperUpdate::NotShared
    })
}
