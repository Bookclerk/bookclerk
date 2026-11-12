//! Store accounts that follow their person between instances ("Accounts
//! on every instance" in `docs/adr/library-catalog-network.md`).
//!
//! - **The operator's rule.** Each instance's operator decides, per
//!   member, whether the member may connect store accounts there
//!   (`member_account_rules`; no row means allowed). A connect on this
//!   instance and an account that arrives for the member from another
//!   library both check it ([`member_may_connect_accounts`]).
//! - **Sending.** When a member lets a branch join and chooses "Bring my
//!   store connections to this branch", the library gathers that member's
//!   own accounts, never another member's, with their secrets unsealed
//!   ([`accounts_to_carry`]), seals them to the branch's box key, and
//!   records where each went ([`record_accounts_sent`]).
//! - **Receiving.** The branch imports them for the person who joined
//!   ([`import_carried_accounts`]): each account, its secrets re-sealed
//!   with this install's key, the link to the person and the record of
//!   where it came from are one atomic batch. An account already connected
//!   here is left as it is; one connected to someone else here is refused.
//!
//! Where each account travelled (`account_shares`) is kept for later
//! steps of the plan: disconnecting an account everywhere and keeping a
//! single-keeper login in step.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_node_proto::{
    check_account_bundle, AccountBundle, CarriedAccount, CarriedSecret, SecretValue,
    ACCOUNT_BUNDLE_VERSION, MAX_ACCOUNT_BUNDLE_BYTES, MAX_BUNDLE_ACCOUNTS,
};
use bookclerk_plugin_abi::{DbValue, TypedDbStatement};
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use zeroize::Zeroizing;

use super::batch::{self, int, text};
use crate::entities::{encrypted_secrets, member_account_rules};
use crate::error::{LibraryError, Result};
use crate::models::{UserRole, UserStatus};
use crate::secrets::{build_sealed_record, get_secret, secret_account_type, unseal_secret};
use crate::store::LibraryStore;

/// `account_shares.direction` of an account this library sent to a branch.
pub const SHARE_SENT: &str = "sent";

/// `account_shares.direction` of an account this install received from a
/// library it joined.
pub const SHARE_RECEIVED: &str = "received";

/// The `account_shares.peer` of a branch node this library sent accounts to.
#[must_use]
pub fn node_peer(node_id: &str) -> String {
    format!("node:{node_id}")
}

/// The `account_shares.peer` of a library this install received accounts
/// from.
#[must_use]
pub fn link_peer(cluster_id: &str) -> String {
    format!("link:{cluster_id}")
}

/// Whether `user_id` may connect store accounts on this instance: an
/// active user with no rule, or whose rule allows it.
///
/// # Errors
///
/// Returns the read error.
pub async fn member_may_connect_accounts(store: &LibraryStore, user_id: i64) -> Result<bool> {
    let Some(user) = store.get_user(user_id).await? else {
        return Ok(false);
    };
    if user.status != UserStatus::Active {
        return Ok(false);
    }
    Ok(member_account_rules::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_none_or(|rule| rule.may_connect != 0))
}

/// Whether the portal identity `identity_id` may connect store accounts
/// here: the rule of the user it belongs to ([`member_may_connect_accounts`]).
/// An identity that belongs to no first-party user (an integration's
/// external user) has no member rule and may connect, as before; an
/// unknown identity may not.
///
/// # Errors
///
/// Returns the read error.
pub async fn identity_may_connect_accounts(store: &LibraryStore, identity_id: i64) -> Result<bool> {
    let Some(identity) = store.get_portal_identity_by_id(identity_id).await? else {
        return Ok(false);
    };
    match identity.user_id {
        Some(user_id) => member_may_connect_accounts(store, user_id).await,
        None => Ok(true),
    }
}

/// Every stored rule, by user id (`true` = may connect). Users with no
/// rule are left out; they may connect.
///
/// # Errors
///
/// Returns the read error.
pub async fn member_account_rules(store: &LibraryStore) -> Result<BTreeMap<i64, bool>> {
    Ok(member_account_rules::Entity::find()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|rule| (rule.user_id, rule.may_connect != 0))
        .collect())
}

/// Sets whether `user_id` may connect store accounts on this instance.
///
/// The caller has already checked that the actor may manage the user. One
/// atomic batch: the stored rule is updated, or inserted when there is
/// none. Accounts the member already connected stay connected.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown user and the write
/// error.
pub async fn set_member_may_connect_accounts(
    store: &LibraryStore,
    user_id: i64,
    allowed: bool,
) -> Result<()> {
    if store.get_user(user_id).await?.is_none() {
        return Err(LibraryError::NotFound(format!("user {user_id}")));
    }
    let now = Utc::now().to_rfc3339();
    let value = i64::from(allowed);
    let operation_id = format!("member-account-rule-{user_id}-{}", uuid::Uuid::new_v4());
    store
        .execute_host_batch(batch::request(
            &operation_id,
            vec![
                batch::exec(
                    "UPDATE member_account_rules SET may_connect = ?, updated_at = ? \
                     WHERE user_id = ?",
                    vec![int(value), text(&now), int(user_id)],
                ),
                batch::exec(
                    "INSERT OR IGNORE INTO member_account_rules (user_id, may_connect, updated_at) \
                     VALUES (?, ?, ?)",
                    vec![int(user_id), int(value), text(&now)],
                ),
            ],
        ))
        .await?;
    Ok(())
}

/// One store account a member connected here, as the join consent page
/// lists it. Carries no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberAccount {
    /// Provenance-qualified plugin key of the storefront.
    pub plugin_key: String,
    /// Store account id.
    pub account_id: String,
    /// Display alias of the storefront.
    pub source: String,
    /// Store marketplace or locale code.
    pub marketplace: String,
    /// The account's display label, when it has one.
    pub label: Option<String>,
}

/// The store accounts `user_id` has connected here: linked to any of the
/// user's portal identities and still connected (`connection_status` is
/// `active`), sorted by plugin key and account id.
///
/// # Errors
///
/// Returns the read error.
pub async fn member_accounts(store: &LibraryStore, user_id: i64) -> Result<Vec<MemberAccount>> {
    let mut linked = BTreeMap::new();
    for identity in store.list_portal_identities_for_user(user_id).await? {
        for link in store.list_account_links(identity.id).await? {
            linked.insert((link.plugin_key.clone(), link.account_id.clone()), link);
        }
    }
    let mut accounts = Vec::new();
    for ((plugin_key, account_id), link) in linked {
        let Some(account) = store.get_account(&plugin_key, &account_id).await? else {
            continue;
        };
        if account.connection_status != "active" {
            continue;
        }
        accounts.push(MemberAccount {
            plugin_key,
            account_id,
            source: link.source,
            marketplace: account.marketplace,
            label: account.label.filter(|label| !label.trim().is_empty()),
        });
    }
    Ok(accounts)
}

/// The store accounts `user_id` connected here that may travel to a
/// branch ([`member_accounts`] whose plugin `shareable` accepts), each
/// with its secrets unsealed. At most [`MAX_BUNDLE_ACCOUNTS`]; an account
/// without a secret is left out (there is nothing to sign in with).
///
/// # Arguments
///
/// * `store` - This library.
/// * `user_id` - The member who chose to bring their connections.
/// * `shareable` - Whether a plugin key's store credentials may be held by
///   several instances (`credentials = "shareable"` in its `plugin.toml`).
///
/// # Errors
///
/// Returns a read error, or the unseal error of a secret.
pub async fn accounts_to_carry(
    store: &LibraryStore,
    user_id: i64,
    shareable: &(dyn Fn(&str) -> bool + Send + Sync),
) -> Result<Vec<CarriedAccount>> {
    let mut carried = Vec::new();
    // Room for the bundle's own fields around the accounts.
    const BUNDLE_OVERHEAD: usize = 1024;
    let mut used = BUNDLE_OVERHEAD;
    for account in member_accounts(store, user_id).await? {
        if !shareable(&account.plugin_key) {
            continue;
        }
        if carried.len() >= MAX_BUNDLE_ACCOUNTS {
            tracing::warn!(
                user_id,
                limit = MAX_BUNDLE_ACCOUNTS,
                "more store accounts than one join carries; the rest stay here"
            );
            break;
        }
        let secrets = match account_secrets(store, &account.plugin_key, &account.account_id).await {
            Ok(secrets) => secrets,
            // One account whose secrets can't be read stays here; the
            // others still travel.
            Err(err) => {
                tracing::warn!(
                    user_id,
                    plugin_key = %account.plugin_key,
                    error = %err,
                    "a store account's secrets could not be read; it stays here"
                );
                continue;
            }
        };
        if secrets.is_empty() {
            continue;
        }
        let mut candidate = CarriedAccount {
            plugin_key: account.plugin_key,
            account_id: account.account_id,
            source: account.source,
            marketplace: account.marketplace,
            label: account.label.map(|label| label.trim().to_owned()),
            secrets,
        };
        if !travels(&candidate) {
            // A label the bundle refuses is left behind, not the account.
            candidate.label = None;
        }
        if !travels(&candidate) {
            tracing::warn!(
                user_id,
                plugin_key = %candidate.plugin_key,
                "a store account does not fit an account bundle; it stays here"
            );
            continue;
        }
        let size = serde_json::to_vec(&candidate).map_or(usize::MAX, |json| json.len());
        if used.saturating_add(size) > MAX_ACCOUNT_BUNDLE_BYTES {
            tracing::warn!(
                user_id,
                plugin_key = %candidate.plugin_key,
                "the store accounts outgrow one join; the rest stay here"
            );
            continue;
        }
        used = used.saturating_add(size).saturating_add(1);
        carried.push(candidate);
    }
    Ok(carried)
}

/// Whether `account` passes the account bundle's checks on its own, so one
/// odd account never stops the others from travelling.
fn travels(account: &CarriedAccount) -> bool {
    const PROBE: &str = "probe";
    check_account_bundle(
        &AccountBundle {
            v: ACCOUNT_BUNDLE_VERSION,
            cluster_id: PROBE.to_owned(),
            node_id: PROBE.to_owned(),
            accounts: vec![account.clone()],
        },
        PROBE,
        PROBE,
    )
    .is_ok()
}

/// Every `integration` secret of one account, unsealed, sorted by kind and
/// name.
async fn account_secrets(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
) -> Result<Vec<CarriedSecret>> {
    let rows = encrypted_secrets::Entity::find()
        .filter(encrypted_secrets::Column::AccountType.eq(secret_account_type::INTEGRATION))
        .filter(encrypted_secrets::Column::Provider.eq(plugin_key))
        .filter(encrypted_secrets::Column::AccountId.eq(account_id))
        .order_by_asc(encrypted_secrets::Column::Kind)
        .order_by_asc(encrypted_secrets::Column::Name)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut secrets = Vec::new();
    for row in rows {
        let Some(record) = get_secret(
            store.db(),
            &row.kind,
            Some(plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
            &row.name,
        )
        .await?
        else {
            continue;
        };
        let value = Zeroizing::new(unseal_secret(&record)?);
        // This process now holds the plaintext to send it: register it so
        // no log line or diagnostics report here can carry it.
        bookclerk_config::register_secret_bytes(&value);
        secrets.push(CarriedSecret {
            kind: record.kind,
            name: record.name,
            value: SecretValue::new(value.to_vec()),
        });
    }
    Ok(secrets)
}

/// Records that `accounts` were sent to the branch `node_id`
/// (`account_shares`, direction [`SHARE_SENT`]), in atomic batches of at
/// most 20 accounts; a repeat refreshes the time.
///
/// # Errors
///
/// Returns the write error.
pub async fn record_accounts_sent(
    store: &LibraryStore,
    node_id: &str,
    accounts: &[CarriedAccount],
) -> Result<()> {
    if accounts.is_empty() {
        return Ok(());
    }
    /// Accounts per batch: two statements each, so a batch stays within
    /// the 40-statement minimum every database guest must accept
    /// (`HOST_MIN_STATEMENTS`).
    const ACCOUNTS_PER_BATCH: usize = 20;
    let now = Utc::now().to_rfc3339();
    let peer = node_peer(node_id);
    for chunk in accounts.chunks(ACCOUNTS_PER_BATCH) {
        let mut statements = Vec::new();
        for account in chunk {
            statements.extend(share_statements(
                &account.plugin_key,
                &account.account_id,
                &peer,
                SHARE_SENT,
                &now,
                None,
            ));
        }
        let operation_id = format!("accounts-sent-{node_id}-{}", uuid::Uuid::new_v4());
        store
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
    }
    Ok(())
}

/// The two statements that record one share (refresh, then insert),
/// each applied only while `guard` (SQL with its binds, `AND`-ed onto the
/// statement's condition) holds.
fn share_statements(
    plugin_key: &str,
    account_id: &str,
    peer: &str,
    direction: &str,
    now: &str,
    guard: Option<(&str, Vec<DbValue>)>,
) -> [TypedDbStatement; 2] {
    let (guard_sql, guard_binds) = guard.map_or((String::new(), Vec::new()), |(sql, binds)| {
        (format!(" AND {sql}"), binds)
    });
    let mut refresh = vec![
        text(now),
        text(plugin_key),
        text(account_id),
        text(peer),
        text(direction),
    ];
    refresh.extend(guard_binds.iter().cloned());
    let mut insert = vec![
        text(plugin_key),
        text(account_id),
        text(peer),
        text(direction),
        text(now),
        text(plugin_key),
        text(account_id),
    ];
    insert.extend(guard_binds);
    [
        batch::exec(
            &format!(
                "UPDATE account_shares SET shared_at = ? \
                 WHERE plugin_key = ? AND account_id = ? AND peer = ? AND direction = ?{guard_sql}"
            ),
            refresh,
        ),
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO account_shares \
                 (plugin_key, account_id, peer, direction, shared_at) \
                 SELECT ?, ?, ?, ?, ? WHERE EXISTS ( \
                    SELECT 1 FROM accounts WHERE plugin_key = ? AND account_id = ?){guard_sql}"
            ),
            insert,
        ),
    ]
}

/// The person on this install that store accounts brought from a library
/// belong to: `preferred` (the Owner who elevated into the operator
/// session that ran the join) while that user is still an active Owner,
/// otherwise the only
/// active Owner. `None` when neither settles it, for example on a new
/// install with no Owner yet; the accounts are then not kept.
///
/// # Errors
///
/// Returns the read error.
pub async fn branch_person(store: &LibraryStore, preferred: Option<i64>) -> Result<Option<i64>> {
    if let Some(user_id) = preferred {
        if store
            .get_user(user_id)
            .await?
            .is_some_and(|user| user.status == UserStatus::Active && user.role == UserRole::Owner)
        {
            return Ok(Some(user_id));
        }
    }
    let owners: Vec<i64> = store
        .list_users()
        .await?
        .into_iter()
        .filter(|user| user.role == UserRole::Owner && user.status == UserStatus::Active)
        .map(|user| user.id)
        .collect();
    Ok(match owners.as_slice() {
        [only] => Some(*only),
        _ => None,
    })
}

/// What [`import_carried_accounts`] did with each account it was handed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarriedImport {
    /// `(plugin_key, account_id)` of each account now connected here for
    /// the person.
    pub imported: Vec<(String, String)>,
    /// Accounts the person had already connected here; left as they are.
    pub already_here: Vec<(String, String)>,
    /// Accounts connected to someone else here; refused.
    pub taken: Vec<(String, String)>,
    /// Accounts that came without a secret, or whose write failed (logged);
    /// nothing of them was stored.
    pub failed: Vec<(String, String)>,
    /// This instance's operator does not let the person connect store
    /// accounts here; nothing was stored. A rule turned off while the
    /// import runs instead stops it: the accounts not yet kept land in
    /// [`Self::failed`].
    pub refused_by_rule: bool,
}

/// Imports the accounts a library sent with its person, for `user_id`,
/// recording that they came from the library `cluster_id`.
///
/// Refused whole, storing nothing, when this instance's rule does not let
/// the person connect accounts ([`member_may_connect_accounts`]). Each
/// batch checks the rule again, so a rule turned off midway stops the
/// accounts not yet kept. Each
/// other account is one atomic batch that inserts the account (scanning
/// on, connected), writes its secrets re-sealed with this install's key,
/// links it to the person's local identity and records the share. Every
/// statement is guarded on the account having no link yet, so an account
/// connected here before (to the person or to someone else) is never
/// overwritten, even when it is connected while the import runs.
///
/// # Errors
///
/// Returns a read error from before any account is written. An account
/// whose own write fails is reported in [`CarriedImport::failed`].
pub async fn import_carried_accounts(
    store: &LibraryStore,
    user_id: i64,
    cluster_id: &str,
    accounts: &[CarriedAccount],
) -> Result<CarriedImport> {
    let mut report = CarriedImport::default();
    if accounts.is_empty() {
        return Ok(report);
    }
    if !member_may_connect_accounts(store, user_id).await? {
        report.refused_by_rule = true;
        return Ok(report);
    }
    let user = store
        .get_user(user_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("user {user_id}")))?;
    let identity = store
        .ensure_local_portal_identity(user_id, user.display_name.as_deref())
        .await?;
    let mut mine = BTreeSet::new();
    for identity in store.list_portal_identities_for_user(user_id).await? {
        for link in store.list_account_links(identity.id).await? {
            mine.insert((link.plugin_key, link.account_id));
        }
    }
    let peer = link_peer(cluster_id);
    for (index, account) in accounts.iter().enumerate() {
        let key = (account.plugin_key.clone(), account.account_id.clone());
        if mine.contains(&key) {
            report.already_here.push(key);
            continue;
        }
        match store
            .count_account_links_for_account(&account.plugin_key, &account.account_id)
            .await
        {
            Ok(0) => {}
            Ok(_) => {
                report.taken.push(key);
                continue;
            }
            // A read that fails counts against this account only; what
            // was already kept stays in the report.
            Err(err) => {
                tracing::warn!(
                    plugin_key = %account.plugin_key,
                    error = %err,
                    "a brought store account could not be checked; it was not kept"
                );
                report.failed.push(key);
                continue;
            }
        }
        if account.secrets.is_empty() {
            // Nothing to sign in with: never a connected account without
            // a credential.
            report.failed.push(key);
            continue;
        }
        match import_one(store, user_id, identity.id, &peer, account).await {
            Ok(true) => report.imported.push(key),
            // The batch stored nothing: either someone linked the account
            // meanwhile, or the operator turned the person's rule off (or
            // disabled them) while the import ran. In that case this and
            // every later account are not kept.
            Ok(false) => {
                if member_may_connect_accounts(store, user_id)
                    .await
                    .unwrap_or(false)
                {
                    report.taken.push(key);
                } else {
                    tracing::warn!(
                        "store connections stopped being allowed while they were kept; \
                         the rest were not kept"
                    );
                    report.failed.push(key);
                    report.failed.extend(
                        accounts
                            .iter()
                            .skip(index + 1)
                            .map(|rest| (rest.plugin_key.clone(), rest.account_id.clone())),
                    );
                    break;
                }
            }
            // One account that can't be written doesn't lose the others,
            // nor the report of what was kept.
            Err(err) => {
                // A batch can commit and still answer an error (a lost
                // reply): the link tells whether this account was kept.
                let kept = store
                    .list_account_links(identity.id)
                    .await
                    .is_ok_and(|links| {
                        links.iter().any(|link| {
                            link.plugin_key == account.plugin_key
                                && link.account_id == account.account_id
                        })
                    });
                if kept {
                    report.imported.push(key);
                } else {
                    tracing::warn!(
                        plugin_key = %account.plugin_key,
                        error = %err,
                        "a store account brought from the library could not be kept"
                    );
                    report.failed.push(key);
                }
            }
        }
    }
    Ok(report)
}

/// Takes back a store account a refused login stored: deletes its secrets
/// and marks it `revoked` with scans off, in one atomic batch whose every
/// statement applies only while no link holds the account.
///
/// A link committed before the batch runs (another login of the same
/// store account that finished meanwhile) keeps the account and its
/// secrets as they are.
///
/// # Errors
///
/// Returns the write error.
pub async fn revoke_unlinked_account(
    store: &LibraryStore,
    plugin_key: &str,
    account_id: &str,
) -> Result<()> {
    const UNLINKED: &str = "NOT EXISTS (SELECT 1 FROM account_links \
                            WHERE plugin_key = ? AND account_id = ?)";
    let now = Utc::now().to_rfc3339();
    let statements = vec![
        batch::exec(
            &format!(
                "DELETE FROM encrypted_secrets \
                 WHERE account_type = ? AND provider = ? AND account_id = ? AND {UNLINKED}"
            ),
            vec![
                text(secret_account_type::INTEGRATION),
                text(plugin_key),
                text(account_id),
                text(plugin_key),
                text(account_id),
            ],
        ),
        batch::exec(
            &format!(
                "UPDATE accounts SET scan_enabled = 0, connection_status = 'revoked', \
                 updated_at = ? WHERE plugin_key = ? AND account_id = ? AND {UNLINKED}"
            ),
            vec![
                text(&now),
                text(plugin_key),
                text(account_id),
                text(plugin_key),
                text(account_id),
            ],
        ),
    ];
    let operation_id = format!("refused-login-{}", uuid::Uuid::new_v4());
    store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    crate::secrets::cache_invalidate_account(account_id);
    Ok(())
}

/// One account's import batch; `false` when, by the time the batch ran,
/// the account was linked or `user_id` could no longer connect accounts.
///
/// Every statement before the link is guarded on both: no link holds the
/// account yet, and the user is active and not barred by a
/// `member_account_rules` row. An operator who turns the rule off while
/// an import runs therefore stops every batch that has not committed.
pub(super) async fn import_one(
    store: &LibraryStore,
    user_id: i64,
    identity_id: i64,
    peer: &str,
    account: &CarriedAccount,
) -> Result<bool> {
    const UNLINKED: &str = "NOT EXISTS (SELECT 1 FROM account_links \
                            WHERE plugin_key = ? AND account_id = ?) \
                            AND EXISTS (SELECT 1 FROM users WHERE id = ? AND status = 'active') \
                            AND NOT EXISTS (SELECT 1 FROM member_account_rules \
                            WHERE user_id = ? AND may_connect = 0)";
    let now = Utc::now().to_rfc3339();
    let key = || {
        vec![
            text(&account.plugin_key),
            text(&account.account_id),
            int(user_id),
            int(user_id),
        ]
    };
    let with_key = |mut values: Vec<DbValue>| {
        values.extend(key());
        values
    };
    let mut statements = vec![
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO accounts \
                 (plugin_key, account_id, marketplace, label, scan_enabled, source, \
                  connection_status, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, 1, ?, 'active', ?, ? WHERE {UNLINKED}"
            ),
            with_key(vec![
                text(&account.plugin_key),
                text(&account.account_id),
                text(&account.marketplace),
                batch::text_opt(account.label.as_deref()),
                text(&account.source),
                text(&now),
                text(&now),
            ]),
        ),
        batch::exec(
            &format!(
                "UPDATE accounts SET marketplace = ?, label = COALESCE(?, label), source = ?, \
                 scan_enabled = 1, connection_status = 'active', updated_at = ? \
                 WHERE plugin_key = ? AND account_id = ? AND {UNLINKED}"
            ),
            with_key(vec![
                text(&account.marketplace),
                batch::text_opt(account.label.as_deref()),
                text(&account.source),
                text(&now),
                text(&account.plugin_key),
                text(&account.account_id),
            ]),
        ),
    ];
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
        let sealed = || {
            vec![
                text(&record.format),
                batch::blob(&record.ciphertext),
                batch::text_opt(record.cipher_algorithm.as_deref()),
                batch::blob_opt(record.cipher_nonce.as_deref()),
            ]
        };
        let names = || {
            vec![
                text(&record.kind),
                text(&account.plugin_key),
                text(secret_account_type::INTEGRATION),
                text(&account.account_id),
                text(&record.name),
            ]
        };
        let mut update = sealed();
        update.push(text(&now));
        update.extend(names());
        statements.push(batch::exec(
            &format!(
                "UPDATE encrypted_secrets SET format = ?, ciphertext = ?, cipher_algorithm = ?, \
                 cipher_nonce = ?, kdf_algorithm = NULL, kdf_salt = NULL, kdf_m_cost = NULL, \
                 kdf_t_cost = NULL, kdf_p_cost = NULL, updated_at = ? \
                 WHERE kind = ? AND provider = ? AND account_type = ? AND account_id = ? \
                 AND name = ? AND {UNLINKED}"
            ),
            with_key(update),
        ));
        let mut insert = names();
        insert.extend(sealed());
        insert.push(text(&now));
        insert.push(text(&now));
        statements.push(batch::exec(
            &format!(
                "INSERT OR IGNORE INTO encrypted_secrets \
                 (kind, provider, account_type, account_id, name, format, ciphertext, \
                  cipher_algorithm, cipher_nonce, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE {UNLINKED}"
            ),
            with_key(insert),
        ));
        records.push(record);
    }
    // The link goes after every statement guarded on "no link yet".
    let link_index = statements.len();
    statements.push(batch::exec(
        &format!(
            "INSERT OR IGNORE INTO account_links \
             (identity_id, plugin_key, account_id, source, created_at) \
             SELECT ?, ?, ?, ?, ? WHERE {UNLINKED}"
        ),
        with_key(vec![
            int(identity_id),
            text(&account.plugin_key),
            text(&account.account_id),
            text(&account.source),
            text(&now),
        ]),
    ));
    // Recorded only once the account is the person's.
    statements.extend(share_statements(
        &account.plugin_key,
        &account.account_id,
        peer,
        SHARE_RECEIVED,
        &now,
        Some((
            "EXISTS (SELECT 1 FROM account_links \
             WHERE identity_id = ? AND plugin_key = ? AND account_id = ?)",
            vec![
                int(identity_id),
                text(&account.plugin_key),
                text(&account.account_id),
            ],
        )),
    ));
    let operation_id = format!("carried-account-{}", uuid::Uuid::new_v4());
    let reply = store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    for record in &records {
        crate::secrets::forget_cached_secret(record);
    }
    Ok(batch::rows_affected(&reply, link_index) == 1)
}
