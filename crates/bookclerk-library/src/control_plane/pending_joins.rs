//! On a host joining a library with a sign-in (the OAuth code + PKCE join):
//! the sign-ins in progress.
//!
//! [`create_pending_join`] keeps everything the joining daemon must not
//! hand to the browser: the PKCE verifier and the node's Ed25519 and X25519
//! private keys are sealed with this install's DEK into one
//! `encrypted_secrets` row (`kind = cluster_join`), and the `pending_joins`
//! row stores only `hash_token(state)` and the public facts of the join.
//! [`take_pending_join`] consumes the row once, when the browser comes back
//! with the owner's code and the same `state`; a row older than
//! [`PENDING_JOIN_TTL`] is refused and purged.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use zeroize::Zeroizing;

use super::cluster_links::{decode_spki_pin, ClusterLinkKeys};
use crate::entities::pending_joins;
use crate::error::{LibraryError, Result};
use crate::secrets::{
    build_sealed_record, delete_secret, get_secret, secret_account_type, secret_kind,
    unseal_secret, upsert_secret,
};
use crate::store::LibraryStore;
use crate::token_hash::hash_token;

/// How long a sign-in may take before its pending row is refused.
pub const PENDING_JOIN_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// `encrypted_secrets.name` of a pending join's sealed material.
const PENDING_JOIN_SECRET_NAME: &str = "pending_join";

/// Length of each private key.
const KEY_LEN: usize = 32;

/// Longest PKCE verifier stored (RFC 7636 allows up to 128 characters).
const MAX_VERIFIER_BYTES: usize = 128;

/// Longest branch or library name stored.
pub const MAX_JOIN_NAME_BYTES: usize = 64;

/// Fixed-width UTC timestamp, so stored times compare correctly as text.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// What a joining daemon records when it starts a sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPendingJoin {
    /// Owner origin the sign-in goes to.
    pub owner_origin: String,
    /// SPKI pin the operator confirmed for a self-signed owner.
    pub spki_pin: Option<String>,
    /// Owner cluster id `GET /api/node/v1/info` reported.
    pub owner_cluster_id: String,
    /// Library name the owner reported.
    pub library_name: String,
    /// Name this install joins as.
    pub branch_name: String,
    /// Redirect URI of the authorization request.
    pub redirect_uri: String,
    /// True when the operator offered to join as a clerk (the install was
    /// empty); the callback may then apply a clerk bootstrap.
    pub allow_clerk: bool,
}

/// A pending join as stored (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingJoin {
    /// Owner origin.
    pub owner_origin: String,
    /// Confirmed SPKI pin.
    pub spki_pin: Option<String>,
    /// Owner cluster id.
    pub owner_cluster_id: String,
    /// Library name.
    pub library_name: String,
    /// Branch name.
    pub branch_name: String,
    /// Redirect URI.
    pub redirect_uri: String,
    /// True when the operator offered to join as a clerk.
    pub allow_clerk: bool,
    /// RFC 3339 expiry.
    pub expires_at: String,
}

impl From<pending_joins::Model> for PendingJoin {
    fn from(row: pending_joins::Model) -> Self {
        Self {
            owner_origin: row.owner_origin,
            spki_pin: row.spki_pin,
            owner_cluster_id: row.owner_cluster_id,
            library_name: row.library_name,
            branch_name: row.branch_name,
            redirect_uri: row.redirect_uri,
            allow_clerk: row.allow_clerk != 0,
            expires_at: row.expires_at,
        }
    }
}

/// A consumed pending join with its secrets. `Debug` prints no secret.
pub struct TakenJoin {
    /// The stored facts.
    pub join: PendingJoin,
    /// The PKCE verifier.
    pub verifier: Zeroizing<String>,
    /// The node keys generated for the join.
    pub keys: ClusterLinkKeys,
}

impl std::fmt::Debug for TakenJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TakenJoin")
            .field("join", &self.join)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

/// True for a short display name without control characters.
fn is_display_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_JOIN_NAME_BYTES
        && !name.chars().any(char::is_control)
}

/// Records a sign-in in progress: seals `verifier` and `keys` and writes the
/// row under `hash_token(state)`. Expired rows are purged first.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a malformed origin, pin, name or
/// verifier, an error when no master key is configured, and the write
/// error.
pub async fn create_pending_join(
    store: &LibraryStore,
    join: &NewPendingJoin,
    state: &str,
    verifier: &str,
    keys: &ClusterLinkKeys,
) -> Result<PendingJoin> {
    purge_expired_pending_joins(store).await?;
    let origin = bookclerk_node_proto::canonical_origin(&join.owner_origin)
        .ok_or_else(|| LibraryError::Invalid("owner origin must be an http(s) origin".into()))?;
    if let Some(pin) = join.spki_pin.as_deref() {
        decode_spki_pin(pin)?;
    }
    if !is_display_name(&join.branch_name) || !is_display_name(&join.library_name) {
        return Err(LibraryError::Invalid(format!(
            "branch and library names must be 1 to {MAX_JOIN_NAME_BYTES} bytes without control characters"
        )));
    }
    if state.len() < 16 || verifier.is_empty() || verifier.len() > MAX_VERIFIER_BYTES {
        return Err(LibraryError::Invalid("malformed state or verifier".into()));
    }
    let state_hash = hash_token(state);
    let mut plaintext = Zeroizing::new(Vec::with_capacity(2 * KEY_LEN + verifier.len()));
    plaintext.extend_from_slice(keys.sign_seed());
    plaintext.extend_from_slice(keys.box_secret());
    plaintext.extend_from_slice(verifier.as_bytes());
    let record = build_sealed_record(
        &plaintext,
        secret_kind::CLUSTER_JOIN,
        &join.owner_cluster_id,
        secret_account_type::OPERATOR,
        &state_hash,
        PENDING_JOIN_SECRET_NAME,
    )?;
    upsert_secret(store.db(), &record).await?;
    let now = Utc::now();
    let ttl = Duration::from_std(PENDING_JOIN_TTL)
        .map_err(|_| LibraryError::Invalid("pending join ttl out of range".into()))?;
    let row = pending_joins::ActiveModel {
        state_hash: Set(state_hash),
        owner_origin: Set(origin),
        spki_pin: Set(join.spki_pin.clone()),
        owner_cluster_id: Set(join.owner_cluster_id.clone()),
        library_name: Set(join.library_name.trim().to_string()),
        branch_name: Set(join.branch_name.trim().to_string()),
        redirect_uri: Set(join.redirect_uri.clone()),
        allow_clerk: Set(i64::from(join.allow_clerk)),
        created_at: Set(stamp(now)),
        expires_at: Set(stamp(now + ttl)),
    };
    pending_joins::Entity::insert(row)
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    pending_joins::Entity::find_by_id(hash_token(state))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(PendingJoin::from)
        .ok_or_else(|| LibraryError::NotFound("pending join".into()))
}

/// Consumes the pending join for `state`: answers its facts and secrets once
/// and deletes both rows. `None` for an unknown, already consumed, or
/// expired state.
///
/// # Errors
///
/// Returns the read or write error, or [`LibraryError::Invalid`] when the
/// sealed material is malformed.
pub async fn take_pending_join(store: &LibraryStore, state: &str) -> Result<Option<TakenJoin>> {
    let state_hash = hash_token(state);
    let Some(row) = pending_joins::Entity::find_by_id(state_hash.clone())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    // Single use: only the caller whose delete removed the row goes on.
    let deleted = pending_joins::Entity::delete_many()
        .filter(pending_joins::Column::StateHash.eq(&state_hash))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if deleted.rows_affected != 1 {
        return Ok(None);
    }
    let record = get_secret(
        store.db(),
        secret_kind::CLUSTER_JOIN,
        Some(&row.owner_cluster_id),
        secret_account_type::OPERATOR,
        Some(&state_hash),
        PENDING_JOIN_SECRET_NAME,
    )
    .await?;
    delete_secret(
        store.db(),
        secret_kind::CLUSTER_JOIN,
        Some(&row.owner_cluster_id),
        secret_account_type::OPERATOR,
        Some(&state_hash),
        PENDING_JOIN_SECRET_NAME,
    )
    .await?;
    if row.expires_at <= stamp(Utc::now()) {
        return Ok(None);
    }
    let Some(record) = record else {
        return Ok(None);
    };
    let plaintext = Zeroizing::new(unseal_secret(&record)?);
    if plaintext.len() <= 2 * KEY_LEN {
        return Err(LibraryError::Invalid(
            "pending join secrets are malformed".into(),
        ));
    }
    let mut sign_seed = [0u8; KEY_LEN];
    let mut box_secret = [0u8; KEY_LEN];
    sign_seed.copy_from_slice(&plaintext[..KEY_LEN]);
    box_secret.copy_from_slice(&plaintext[KEY_LEN..2 * KEY_LEN]);
    let keys = ClusterLinkKeys::from_bytes(&sign_seed, &box_secret);
    zeroize::Zeroize::zeroize(&mut sign_seed);
    zeroize::Zeroize::zeroize(&mut box_secret);
    let verifier = std::str::from_utf8(&plaintext[2 * KEY_LEN..])
        .map_err(|_| LibraryError::Invalid("pending join verifier is malformed".into()))?;
    Ok(Some(TakenJoin {
        join: PendingJoin::from(row),
        verifier: Zeroizing::new(verifier.to_string()),
        keys,
    }))
}

/// Deletes every expired pending join and its sealed material. Answers how
/// many rows went.
///
/// # Errors
///
/// Returns the read or write error.
pub async fn purge_expired_pending_joins(store: &LibraryStore) -> Result<u64> {
    let now = stamp(Utc::now());
    let expired = pending_joins::Entity::find()
        .filter(pending_joins::Column::ExpiresAt.lte(&now))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut purged = 0;
    for row in expired {
        delete_secret(
            store.db(),
            secret_kind::CLUSTER_JOIN,
            Some(&row.owner_cluster_id),
            secret_account_type::OPERATOR,
            Some(&row.state_hash),
            PENDING_JOIN_SECRET_NAME,
        )
        .await?;
        let deleted = pending_joins::Entity::delete_many()
            .filter(pending_joins::Column::StateHash.eq(&row.state_hash))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        purged += deleted.rows_affected;
    }
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master_key::master_key_test_lock_async;
    use sea_orm::sea_query::Expr;

    async fn store_in(dir: &std::path::Path) -> LibraryStore {
        let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
            .await
            .expect("open sqlite file");
        crate::apply_host_schema(&db).await.expect("schema");
        crate::configure_master_key(dir).expect("master key");
        LibraryStore::from_connection(db)
    }

    fn new_join() -> NewPendingJoin {
        NewPendingJoin {
            owner_origin: "HTTPS://Owner.Example:443".into(),
            spki_pin: None,
            owner_cluster_id: "cluster-1".into(),
            library_name: "owner.example".into(),
            branch_name: "den".into(),
            redirect_uri: "http://localhost:8787/join/callback".into(),
            allow_clerk: false,
        }
    }

    #[tokio::test]
    async fn a_pending_join_is_taken_once_with_its_sealed_secrets() {
        let _lock = master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let keys = ClusterLinkKeys::generate();
        let state = "s".repeat(43);
        let stored = create_pending_join(&store, &new_join(), &state, "verifier-1", &keys)
            .await
            .unwrap();
        assert_eq!(stored.owner_origin, "https://owner.example");
        // Neither the state nor the verifier is stored in plain form.
        let rows = pending_joins::Entity::find().all(store.db()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].state_hash, state);
        assert!(!format!("{rows:?}").contains("verifier-1"));

        assert!(take_pending_join(&store, "other-state-value-xx")
            .await
            .unwrap()
            .is_none());
        let taken = take_pending_join(&store, &state).await.unwrap().unwrap();
        assert_eq!(taken.verifier.as_str(), "verifier-1");
        assert_eq!(taken.keys.sign_public_key(), keys.sign_public_key());
        assert_eq!(taken.keys.box_public_key(), keys.box_public_key());
        assert!(!format!("{taken:?}").contains("verifier-1"));
        assert!(take_pending_join(&store, &state).await.unwrap().is_none());
        let secrets = crate::secrets::list_secrets(store.db(), secret_kind::CLUSTER_JOIN)
            .await
            .unwrap();
        assert!(secrets.is_empty(), "the sealed material goes with the row");
    }

    #[tokio::test]
    async fn an_expired_pending_join_is_refused_and_purged() {
        let _lock = master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let state = "t".repeat(43);
        create_pending_join(
            &store,
            &new_join(),
            &state,
            "v",
            &ClusterLinkKeys::generate(),
        )
        .await
        .unwrap();
        pending_joins::Entity::update_many()
            .col_expr(
                pending_joins::Column::ExpiresAt,
                Expr::value("2000-01-01T00:00:00.000000Z"),
            )
            .exec(store.db())
            .await
            .unwrap();
        assert!(take_pending_join(&store, &state).await.unwrap().is_none());

        let again = "u".repeat(43);
        create_pending_join(
            &store,
            &new_join(),
            &again,
            "v",
            &ClusterLinkKeys::generate(),
        )
        .await
        .unwrap();
        pending_joins::Entity::update_many()
            .col_expr(
                pending_joins::Column::ExpiresAt,
                Expr::value("2000-01-01T00:00:00.000000Z"),
            )
            .exec(store.db())
            .await
            .unwrap();
        assert_eq!(purge_expired_pending_joins(&store).await.unwrap(), 1);
        assert!(
            crate::secrets::list_secrets(store.db(), secret_kind::CLUSTER_JOIN)
                .await
                .unwrap()
                .is_empty()
        );

        let mut bad = new_join();
        bad.branch_name = "a\nb".into();
        assert!(matches!(
            create_pending_join(&store, &bad, &again, "v", &ClusterLinkKeys::generate()).await,
            Err(LibraryError::Invalid(_))
        ));
    }
}
