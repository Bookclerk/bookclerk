//! On a host node: the owner clusters this install enrolled into, and the
//! node keys it enrolled with (H-4 design, sections 2 and 8).
//!
//! Joining a cluster generates an Ed25519 signing seed and an X25519 box
//! secret ([`ClusterLinkKeys::generate`]). After the owner redeems the
//! link, [`record_cluster_link`] seals both private keys with this
//! install's DEK into one `encrypted_secrets` row (`kind =
//! cluster_link`, `provider` = the owner cluster id, `account_type =
//! operator`, `account_id` = the node id the owner minted) and writes the
//! `cluster_links` row. The host client reads them back with
//! [`load_cluster_link_keys`] to answer the owner's challenge and to open
//! the secrets the owner seals to the box key.
//!
//! The keys never leave this module in plain form except as zeroizing
//! buffers, and their `Debug` output is redacted.

use chrono::{SecondsFormat, Utc};
use rand::RngCore as _;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QueryTrait};
use serde::Serialize;
use zeroize::Zeroizing;

use crate::entities::cluster_links;
use crate::error::{LibraryError, Result};
use crate::secrets::{
    build_sealed_record, delete_secret, get_secret, secret_account_type, secret_kind,
    unseal_secret, upsert_secret,
};
use crate::store::LibraryStore;

/// `cluster_links.state` of a link the host client connects.
pub const CLUSTER_LINK_ACTIVE: &str = "active";

/// `cluster_links.state` once the owner revoked this node (close 4403).
pub const CLUSTER_LINK_REVOKED: &str = "revoked";

/// `cluster_links.state` after this install left the cluster.
pub const CLUSTER_LINK_LEFT: &str = "left";

/// `encrypted_secrets.name` of a link's node keys.
pub const CLUSTER_LINK_KEYS_NAME: &str = "node_keys";

/// Longest node or cluster id stored.
const MAX_ID_BYTES: usize = 128;

/// Length of each private key.
const KEY_LEN: usize = 32;

/// The node keys of one cluster link: the Ed25519 signing seed and the
/// X25519 box secret. Zeroized on drop; `Debug` shows only the public keys.
#[derive(Clone)]
pub struct ClusterLinkKeys {
    /// Ed25519 signing seed.
    sign_seed: Zeroizing<[u8; KEY_LEN]>,
    /// X25519 box secret.
    box_secret: Zeroizing<[u8; KEY_LEN]>,
}

impl ClusterLinkKeys {
    /// Fresh random keys from the OS RNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut sign_seed = Zeroizing::new([0u8; KEY_LEN]);
        let mut box_secret = Zeroizing::new([0u8; KEY_LEN]);
        rand::rngs::OsRng.fill_bytes(sign_seed.as_mut_slice());
        rand::rngs::OsRng.fill_bytes(box_secret.as_mut_slice());
        Self {
            sign_seed,
            box_secret,
        }
    }

    /// Keys from known bytes (tests, and reading the sealed row back).
    #[must_use]
    pub fn from_bytes(sign_seed: &[u8; KEY_LEN], box_secret: &[u8; KEY_LEN]) -> Self {
        Self {
            sign_seed: Zeroizing::new(*sign_seed),
            box_secret: Zeroizing::new(*box_secret),
        }
    }

    /// The Ed25519 signing seed.
    #[must_use]
    pub fn sign_seed(&self) -> &[u8; KEY_LEN] {
        &self.sign_seed
    }

    /// The X25519 box secret.
    #[must_use]
    pub fn box_secret(&self) -> &[u8; KEY_LEN] {
        &self.box_secret
    }

    /// The Ed25519 public key the owner stores.
    #[must_use]
    pub fn sign_public_key(&self) -> [u8; KEY_LEN] {
        bookclerk_node_proto::sign_public_key(&self.sign_seed)
    }

    /// The X25519 public key the owner seals job secrets to.
    #[must_use]
    pub fn box_public_key(&self) -> [u8; KEY_LEN] {
        bookclerk_node_proto::box_public_key(&self.box_secret)
    }
}

impl std::fmt::Debug for ClusterLinkKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterLinkKeys")
            .field("sign_public_key", &hex(&self.sign_public_key()))
            .field("box_public_key", &hex(&self.box_public_key()))
            .finish_non_exhaustive()
    }
}

/// Lowercase hex of public bytes.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One owner cluster this install enrolled into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterLink {
    /// Owner cluster id.
    pub owner_cluster_id: String,
    /// Owner origin as [`bookclerk_node_proto::canonical_origin`] spells it.
    pub owner_origin: String,
    /// Node id the owner minted for this install.
    pub remote_node_id: String,
    /// SPKI pin of the owner's self-signed certificate, when the link
    /// carried one.
    pub spki_pin: Option<String>,
    /// [`CLUSTER_LINK_ACTIVE`], [`CLUSTER_LINK_REVOKED`], or
    /// [`CLUSTER_LINK_LEFT`].
    pub state: String,
    /// RFC 3339 enrollment time.
    pub enrolled_at: String,
}

impl ClusterLink {
    /// True when the host client should connect this link.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == CLUSTER_LINK_ACTIVE
    }
}

impl From<cluster_links::Model> for ClusterLink {
    fn from(row: cluster_links::Model) -> Self {
        Self {
            owner_cluster_id: row.owner_cluster_id,
            owner_origin: row.owner_origin,
            remote_node_id: row.remote_node_id,
            spki_pin: row.spki_pin,
            state: row.state,
            enrolled_at: row.enrolled_at,
        }
    }
}

/// What enrollment returned, to record with [`record_cluster_link`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClusterLink {
    /// Owner cluster id from the enroll answer.
    pub owner_cluster_id: String,
    /// Owner origin the host enrolled through.
    pub owner_origin: String,
    /// Node id from the enroll answer.
    pub remote_node_id: String,
    /// SPKI pin from the enrollment link.
    pub spki_pin: Option<String>,
}

/// True for an id the owner could have minted (and the handshake accepts).
fn is_plain_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_ID_BYTES
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Decodes an SPKI pin: base64url (no padding) of a SHA-256 digest.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for anything else.
pub fn decode_spki_pin(pin: &str) -> Result<[u8; 32]> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(pin.trim())
        .ok()
        .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        .ok_or_else(|| LibraryError::Invalid("SPKI pin is not a base64url SHA-256 digest".into()))
}

/// Seals `keys` and records the link, replacing an earlier link to the same
/// cluster (a re-enrollment) as `active`.
///
/// The key row is written first: a link row never exists without keys, and
/// a key row left by a failed write is replaced by the next enrollment.
/// When the re-enrollment minted a different node id, the earlier node's
/// key row is deleted once the new link row is stored, so the sealed
/// private keys of a node the owner no longer knows are not kept.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a malformed id, origin, or pin, an
/// error when no master key is configured, and the write error.
pub async fn record_cluster_link(
    store: &LibraryStore,
    link: &NewClusterLink,
    keys: &ClusterLinkKeys,
) -> Result<ClusterLink> {
    if !is_plain_id(&link.owner_cluster_id) || !is_plain_id(&link.remote_node_id) {
        return Err(LibraryError::Invalid(
            "cluster and node ids must be plain ids".into(),
        ));
    }
    let origin = bookclerk_node_proto::canonical_origin(&link.owner_origin)
        .ok_or_else(|| LibraryError::Invalid("owner origin must be an http(s) origin".into()))?;
    if let Some(pin) = link.spki_pin.as_deref() {
        decode_spki_pin(pin)?;
    }
    let mut plaintext = Zeroizing::new(Vec::with_capacity(2 * KEY_LEN));
    plaintext.extend_from_slice(keys.sign_seed());
    plaintext.extend_from_slice(keys.box_secret());
    let record = build_sealed_record(
        &plaintext,
        secret_kind::CLUSTER_LINK,
        &link.owner_cluster_id,
        secret_account_type::OPERATOR,
        &link.remote_node_id,
        CLUSTER_LINK_KEYS_NAME,
    )?;
    let previous = load_cluster_link(store, &link.owner_cluster_id).await?;
    upsert_secret(store.db(), &record).await?;
    let row = cluster_links::ActiveModel {
        owner_cluster_id: Set(link.owner_cluster_id.clone()),
        owner_origin: Set(origin),
        remote_node_id: Set(link.remote_node_id.clone()),
        spki_pin: Set(link.spki_pin.clone()),
        state: Set(CLUSTER_LINK_ACTIVE.into()),
        enrolled_at: Set(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)),
    };
    cluster_links::Entity::insert(row)
        .on_conflict(
            OnConflict::column(cluster_links::Column::OwnerClusterId)
                .update_columns([
                    cluster_links::Column::OwnerOrigin,
                    cluster_links::Column::RemoteNodeId,
                    cluster_links::Column::SpkiPin,
                    cluster_links::Column::State,
                    cluster_links::Column::EnrolledAt,
                ])
                .to_owned(),
        )
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    // The new link is committed by now, so failing to drop the previous
    // node's keys must not report the enrollment failed: the one-use link
    // is spent and a retry would be refused. The stale row is only left
    // behind; nothing reads keys under a node id the link no longer names.
    if let Some(previous) = previous.filter(|prev| prev.remote_node_id != link.remote_node_id) {
        if let Err(err) = delete_secret(
            store.db(),
            secret_kind::CLUSTER_LINK,
            Some(&link.owner_cluster_id),
            secret_account_type::OPERATOR,
            Some(&previous.remote_node_id),
            CLUSTER_LINK_KEYS_NAME,
        )
        .await
        {
            tracing::warn!(
                cluster_id = %link.owner_cluster_id,
                previous_node_id = %previous.remote_node_id,
                error = %err,
                "could not delete the previous node's cluster link keys"
            );
        }
    }
    load_cluster_link(store, &link.owner_cluster_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("cluster link {}", link.owner_cluster_id)))
}

/// Every cluster link, oldest enrollment first.
///
/// # Errors
///
/// Returns the read error.
pub async fn list_cluster_links(store: &LibraryStore) -> Result<Vec<ClusterLink>> {
    Ok(cluster_links::Entity::find()
        .order_by_asc(cluster_links::Column::EnrolledAt)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(ClusterLink::from)
        .collect())
}

/// One cluster link.
///
/// # Errors
///
/// Returns the read error.
pub async fn load_cluster_link(
    store: &LibraryStore,
    owner_cluster_id: &str,
) -> Result<Option<ClusterLink>> {
    Ok(
        cluster_links::Entity::find_by_id(owner_cluster_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(ClusterLink::from),
    )
}

/// Unseals the node keys of an active `link`.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] when the link is not active (a
/// revoked or departed node never signs again),
/// [`LibraryError::NotFound`] when the key row is missing, and
/// [`LibraryError::Invalid`] when it does not hold two 32-byte keys.
pub async fn load_cluster_link_keys(
    store: &LibraryStore,
    link: &ClusterLink,
) -> Result<ClusterLinkKeys> {
    if !link.is_active() {
        return Err(LibraryError::Forbidden(format!(
            "cluster link {} is {}",
            link.owner_cluster_id, link.state
        )));
    }
    let record = get_secret(
        store.db(),
        secret_kind::CLUSTER_LINK,
        Some(&link.owner_cluster_id),
        secret_account_type::OPERATOR,
        Some(&link.remote_node_id),
        CLUSTER_LINK_KEYS_NAME,
    )
    .await?
    .ok_or_else(|| {
        LibraryError::NotFound(format!("node keys of cluster {}", link.owner_cluster_id))
    })?;
    let plaintext = Zeroizing::new(unseal_secret(&record)?);
    if plaintext.len() != 2 * KEY_LEN {
        return Err(LibraryError::Invalid(format!(
            "node keys of cluster {} are malformed",
            link.owner_cluster_id
        )));
    }
    let mut sign_seed = Zeroizing::new([0u8; KEY_LEN]);
    let mut box_secret = Zeroizing::new([0u8; KEY_LEN]);
    sign_seed.copy_from_slice(&plaintext[..KEY_LEN]);
    box_secret.copy_from_slice(&plaintext[KEY_LEN..]);
    Ok(ClusterLinkKeys {
        sign_seed,
        box_secret,
    })
}

/// Sets a link's state. Answers false when no such link exists.
///
/// # Arguments
///
/// * `store` - Library store holding the link.
/// * `owner_cluster_id` - Owner cluster the link joins.
/// * `state` - One of [`CLUSTER_LINK_ACTIVE`], [`CLUSTER_LINK_REVOKED`] or
///   [`CLUSTER_LINK_LEFT`].
/// * `remote_node_id` - When set, only a link still enrolled as this node is
///   changed, so a client running under an earlier enrollment cannot touch the
///   link that replaced it. `None` changes the link whatever its node.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for an unknown state, and the write
/// error.
pub async fn set_cluster_link_state(
    store: &LibraryStore,
    owner_cluster_id: &str,
    state: &str,
    remote_node_id: Option<&str>,
) -> Result<bool> {
    if ![CLUSTER_LINK_ACTIVE, CLUSTER_LINK_REVOKED, CLUSTER_LINK_LEFT].contains(&state) {
        return Err(LibraryError::Invalid(format!(
            "unknown cluster link state {state}"
        )));
    }
    let updated = cluster_links::Entity::update_many()
        .col_expr(cluster_links::Column::State, Expr::value(state))
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .apply_if(remote_node_id, |query, node| {
            query.filter(cluster_links::Column::RemoteNodeId.eq(node))
        })
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(updated.rows_affected == 1)
}

/// Marks an **active** link still enrolled as `remote_node_id`
/// [`CLUSTER_LINK_REVOKED`] (the owner closed with 4403 on that node's
/// connection). A link this install already left stays `left`, and one a
/// later join enrolled as another node keeps its new enrollment.
///
/// # Arguments
///
/// * `store` - Library store holding the link.
/// * `owner_cluster_id` - Owner cluster the link joins.
/// * `remote_node_id` - The node the revoked connection ran as.
///
/// # Returns
///
/// Whether a row changed: `false` when the link is gone, no longer active,
/// or enrolled as another node.
///
/// # Errors
///
/// Returns the write error.
pub async fn mark_cluster_link_revoked(
    store: &LibraryStore,
    owner_cluster_id: &str,
    remote_node_id: &str,
) -> Result<bool> {
    let updated = cluster_links::Entity::update_many()
        .col_expr(
            cluster_links::Column::State,
            Expr::value(CLUSTER_LINK_REVOKED),
        )
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(cluster_links::Column::RemoteNodeId.eq(remote_node_id))
        .filter(cluster_links::Column::State.eq(CLUSTER_LINK_ACTIVE))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(updated.rows_affected == 1)
}

/// Leaves an owner cluster on this side: marks the link
/// [`CLUSTER_LINK_LEFT`] and deletes its sealed node keys, so this install
/// can never sign as that node again. Answers the link as it now stands,
/// or `None` when there is no such link.
///
/// This only records the departure. Telling the owner (`node.leaving`,
/// which revokes the node there) is the caller's, before the keys go:
/// `bookclerk_node_client::announce_leaving` or the running client. A
/// running daemon stops the link's client when it next re-reads the link.
/// Joining again needs a new sign-in or invite.
///
/// Only one enrollment is left: `expected_node_id` when given (the node the
/// caller told the owner about), else the one read here. A link a join
/// enrolled as another node, before this call or while it runs, is refused,
/// so a new enrollment is never marked left with its keys still stored or
/// left without the owner having been told.
///
/// # Arguments
///
/// * `store` - Library store holding the link.
/// * `owner_cluster_id` - Owner cluster the link joins.
/// * `expected_node_id` - The node the caller means to leave, when it read
///   the link earlier (to send `node.leaving`); `None` leaves whatever node
///   the link names now.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] when the link was re-enrolled as
/// another node, and the read or write error.
pub async fn leave_cluster_link(
    store: &LibraryStore,
    owner_cluster_id: &str,
    expected_node_id: Option<&str>,
) -> Result<Option<ClusterLink>> {
    let Some(mut link) = load_cluster_link(store, owner_cluster_id).await? else {
        return Ok(None);
    };
    if expected_node_id.is_some_and(|node| node != link.remote_node_id) {
        return Err(LibraryError::Invalid(
            "the cluster was joined again while leaving it; nothing was left".into(),
        ));
    }
    // State first: a link that is not active is never dialed, even if the
    // key delete below fails.
    if !set_cluster_link_state(
        store,
        owner_cluster_id,
        CLUSTER_LINK_LEFT,
        Some(&link.remote_node_id),
    )
    .await?
    {
        return Err(LibraryError::Invalid(
            "the cluster was joined again while leaving it; nothing was left".into(),
        ));
    }
    delete_secret(
        store.db(),
        secret_kind::CLUSTER_LINK,
        Some(&link.owner_cluster_id),
        secret_account_type::OPERATOR,
        Some(&link.remote_node_id),
        CLUSTER_LINK_KEYS_NAME,
    )
    .await?;
    link.state = CLUSTER_LINK_LEFT.to_string();
    Ok(Some(link))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master_key::master_key_test_lock_async;

    async fn store_in(dir: &std::path::Path) -> LibraryStore {
        let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
            .await
            .expect("open sqlite file");
        crate::apply_host_schema(&db).await.expect("schema");
        crate::configure_master_key(dir).expect("master key");
        LibraryStore::from_connection(db)
    }

    fn new_link(pin: Option<&str>) -> NewClusterLink {
        NewClusterLink {
            owner_cluster_id: "cluster-1".into(),
            owner_origin: "HTTPS://Owner.Example:443/enroll".into(),
            remote_node_id: "node-1".into(),
            spki_pin: pin.map(str::to_string),
        }
    }

    /// Leaving marks the link left and deletes its keys; an unknown
    /// cluster answers `None`.
    #[tokio::test]
    async fn leaving_deletes_the_keys_for_good() {
        let _lock = master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let keys = ClusterLinkKeys::from_bytes(&[3; 32], &[4; 32]);
        record_cluster_link(&store, &new_link(None), &keys)
            .await
            .unwrap();
        assert!(leave_cluster_link(&store, "nope", None)
            .await
            .unwrap()
            .is_none());
        let left = leave_cluster_link(&store, "cluster-1", None)
            .await
            .unwrap()
            .expect("the link");
        assert_eq!(left.state, CLUSTER_LINK_LEFT);
        assert!(!left.is_active());
        assert!(get_secret(
            store.db(),
            secret_kind::CLUSTER_LINK,
            Some("cluster-1"),
            secret_account_type::OPERATOR,
            Some("node-1"),
            CLUSTER_LINK_KEYS_NAME,
        )
        .await
        .unwrap()
        .is_none());
        assert!(load_cluster_link_keys(&store, &left).await.is_err());
    }

    #[tokio::test]
    async fn keys_round_trip_sealed_and_only_while_active() {
        let _lock = master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let keys = ClusterLinkKeys::from_bytes(&[1; 32], &[2; 32]);
        let pin = "A".repeat(43);
        let link = record_cluster_link(&store, &new_link(Some(&pin)), &keys)
            .await
            .unwrap();
        assert_eq!(link.owner_origin, "https://owner.example");
        assert!(link.is_active());
        assert_eq!(
            list_cluster_links(&store).await.unwrap(),
            vec![link.clone()]
        );

        let loaded = load_cluster_link_keys(&store, &link).await.unwrap();
        assert_eq!(loaded.sign_seed(), &[1; 32]);
        assert_eq!(loaded.box_secret(), &[2; 32]);
        assert!(!format!("{loaded:?}").contains(&hex(&[1; 32])));

        // Stored sealed: the raw row holds neither key.
        let record = get_secret(
            store.db(),
            secret_kind::CLUSTER_LINK,
            Some("cluster-1"),
            secret_account_type::OPERATOR,
            Some("node-1"),
            CLUSTER_LINK_KEYS_NAME,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!record
            .ciphertext
            .windows(32)
            .any(|w| w == [1; 32] || w == [2; 32]));

        assert!(
            set_cluster_link_state(&store, "cluster-1", CLUSTER_LINK_REVOKED, None)
                .await
                .unwrap()
        );
        let revoked = load_cluster_link(&store, "cluster-1")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            load_cluster_link_keys(&store, &revoked).await,
            Err(LibraryError::Forbidden(_))
        ));
        assert!(set_cluster_link_state(&store, "cluster-1", "bogus", None)
            .await
            .is_err());

        // Re-enrolling replaces the link and its keys and reactivates it.
        let fresh = ClusterLinkKeys::from_bytes(&[3; 32], &[4; 32]);
        let again = record_cluster_link(&store, &new_link(None), &fresh)
            .await
            .unwrap();
        assert!(again.is_active());
        assert_eq!(again.spki_pin, None);
        let loaded = load_cluster_link_keys(&store, &again).await.unwrap();
        assert_eq!(loaded.sign_seed(), &[3; 32]);
    }

    /// True when a key row is sealed for `node_id` in `cluster-1`.
    async fn key_row_exists(store: &LibraryStore, node_id: &str) -> bool {
        get_secret(
            store.db(),
            secret_kind::CLUSTER_LINK,
            Some("cluster-1"),
            secret_account_type::OPERATOR,
            Some(node_id),
            CLUSTER_LINK_KEYS_NAME,
        )
        .await
        .unwrap()
        .is_some()
    }

    #[tokio::test]
    async fn re_enrolling_as_another_node_deletes_the_old_node_keys() {
        let _lock = master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let first = ClusterLinkKeys::from_bytes(&[1; 32], &[2; 32]);
        record_cluster_link(&store, &new_link(None), &first)
            .await
            .unwrap();
        assert!(key_row_exists(&store, "node-1").await);

        // The owner minted a new node id for the re-enrollment.
        let mut again = new_link(None);
        again.remote_node_id = "node-2".into();
        let fresh = ClusterLinkKeys::from_bytes(&[3; 32], &[4; 32]);
        let link = record_cluster_link(&store, &again, &fresh).await.unwrap();
        assert_eq!(link.remote_node_id, "node-2");
        assert!(key_row_exists(&store, "node-2").await);
        assert!(
            !key_row_exists(&store, "node-1").await,
            "the previous node's sealed keys were left behind"
        );

        // A client still running as node-1 cannot revoke node-2's link.
        assert!(
            !set_cluster_link_state(&store, "cluster-1", CLUSTER_LINK_REVOKED, Some("node-1"))
                .await
                .unwrap()
        );
        let still = load_cluster_link(&store, "cluster-1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            still.is_active(),
            "an old client revoked the replacement link"
        );
        let loaded = load_cluster_link_keys(&store, &link).await.unwrap();
        assert_eq!(loaded.box_secret(), &[4; 32]);

        // Re-enrolling under the same node id keeps (replaces) its row.
        let same = ClusterLinkKeys::from_bytes(&[5; 32], &[6; 32]);
        let link = record_cluster_link(&store, &again, &same).await.unwrap();
        assert!(key_row_exists(&store, "node-2").await);
        let loaded = load_cluster_link_keys(&store, &link).await.unwrap();
        assert_eq!(loaded.sign_seed(), &[5; 32]);
    }

    #[tokio::test]
    async fn malformed_links_are_refused() {
        let _lock = master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let keys = ClusterLinkKeys::generate();
        let mut bad = new_link(Some("not a pin!"));
        assert!(record_cluster_link(&store, &bad, &keys).await.is_err());
        bad.spki_pin = None;
        bad.owner_origin = "ftp://owner.example".into();
        assert!(record_cluster_link(&store, &bad, &keys).await.is_err());
        bad.owner_origin = "https://owner.example".into();
        bad.remote_node_id = "node 1".into();
        assert!(record_cluster_link(&store, &bad, &keys).await.is_err());
        assert!(list_cluster_links(&store).await.unwrap().is_empty());
    }
}
