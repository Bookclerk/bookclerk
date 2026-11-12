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
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::control_plane::{sea_probe, Bump, LiveTopic};
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
    /// "Share this branch's books with *library*" (on by default).
    pub share_books: bool,
    /// "Include books from my own branches" (on by default; off shares
    /// only books acquired on this install itself).
    pub include_branches: bool,
    /// The owner library's ancestry as it last reported it: its own
    /// cluster id first, then each library above it.
    pub ancestry: Vec<String>,
    /// "Download for other members of *library*" (off by default): with it
    /// off the branch takes only its owner's work from the library.
    pub work_for_members: bool,
    /// "Keep copies of other members' books" (off by default): with it off
    /// a book acquired for another member goes only to the library.
    pub keep_member_copies: bool,
    /// The operator's cap on jobs run for this library at once; `None`
    /// leaves the link its equal share of the machine.
    pub max_jobs: Option<LinkJobCap>,
    /// "List my members' books to *library*" (on by default): catalog
    /// lookups from that library are answered with the members' books
    /// (except those of members who opted out) as well as the owner's.
    pub list_members_books: bool,
    /// The local user who joined the library through this link, when one
    /// could be named: the only person who may add store connections to
    /// that library, or withdraw them from it, through this link.
    pub person_user_id: Option<i64>,
}

/// The most jobs a branch runs for one library at once, as its operator
/// set it: 0 (runs nothing for that library) to [`LinkJobCap::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct LinkJobCap(u8);

impl LinkJobCap {
    /// The highest cap an operator may set: the most job slots a branch
    /// ever counts its machine as having.
    pub const MAX: u8 = 16;

    /// A cap of `jobs`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] above [`Self::MAX`].
    pub fn new(jobs: u32) -> Result<Self> {
        u8::try_from(jobs)
            .ok()
            .filter(|jobs| *jobs <= Self::MAX)
            .map(Self)
            .ok_or_else(|| {
                LibraryError::Invalid(format!("a library's job cap is 0 to {} jobs", Self::MAX))
            })
    }

    /// The cap in jobs.
    #[must_use]
    pub fn jobs(self) -> usize {
        usize::from(self.0)
    }

    /// A stored cap; a value outside the range (a hand-edited row) reads as
    /// no cap rather than failing the link.
    fn from_column(raw: Option<i64>) -> Option<Self> {
        raw.and_then(|raw| u32::try_from(raw).ok())
            .and_then(|raw| Self::new(raw).ok())
    }
}

/// `POST /api/config/join/capacity` body: one library and the job cap to
/// set for it.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `CapacityFields` enforces it.
#[serde(deny_unknown_fields)]
pub struct CapacityBody {
    /// The library (owner cluster id).
    #[schema(example = "6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a")]
    cluster_id: String,
    /// The most jobs to run for the library at once (0 runs none); `null`
    /// gives it an equal share of the machine. Required: a body without it
    /// is refused rather than read as `null`.
    #[schema(value_type = Option<u32>, required = true, example = 1, minimum = 0, maximum = 16)]
    max_jobs: Option<Option<u32>>,
}

/// [`CapacityBody`]'s fields as JSON carries them; read only from a JSON
/// object ([`CapacityBody`]'s `Deserialize`), never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityFields {
    /// See [`CapacityBody`].
    cluster_id: String,
    /// See [`CapacityBody`]; `None` when absent, `Some(None)` for `null`.
    #[serde(default, deserialize_with = "present")]
    max_jobs: Option<Option<u32>>,
}

impl<'de> Deserialize<'de> for CapacityBody {
    /// Reads the body from a JSON object only: a derived struct would also
    /// take its fields positionally from an array.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: CapacityFields =
            from_object(deserializer, "a JSON object with cluster_id and max_jobs")?;
        Ok(Self {
            cluster_id: fields.cluster_id,
            max_jobs: fields.max_jobs,
        })
    }
}

/// Reads `T` from a JSON object and nothing else: a derived struct would
/// also take its fields positionally from an array.
///
/// # Arguments
///
/// * `deserializer` - The body's deserializer.
/// * `expecting` - What the error says was expected.
///
/// # Errors
///
/// Returns the deserializer's error for anything but an object `T` reads.
pub(super) fn from_object<'de, D, T>(
    deserializer: D,
    expecting: &'static str,
) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    /// Accepts a map and nothing else.
    struct ObjectOnly<T> {
        /// What the error says was expected.
        expecting: &'static str,
        /// The type read from the map.
        target: std::marker::PhantomData<T>,
    }

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectOnly<T> {
        type Value = T;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.expecting)
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            map: A,
        ) -> std::result::Result<T, A::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(ObjectOnly {
        expecting,
        target: std::marker::PhantomData,
    })
}

/// Reads a field that is present, `null` included, as `Some`; an absent
/// field stays `None` (its `#[serde(default)]`).
///
/// # Errors
///
/// Returns the deserializer's error for a value that is not a number or
/// `null`.
fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A validated job cap change: the library it names (trimmed) and its new
/// cap, `None` for an equal share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCapRequest {
    /// The library (owner cluster id), surrounding whitespace removed.
    pub cluster_id: String,
    /// The cap to store; `None` clears it (an equal share).
    pub max_jobs: Option<LinkJobCap>,
}

impl CapacityBody {
    /// Checks the body: `max_jobs` must be present (`null` included) and at
    /// most [`LinkJobCap::MAX`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when `max_jobs` is missing or above
    /// the maximum.
    pub fn validate(self) -> Result<JobCapRequest> {
        let Some(max_jobs) = self.max_jobs else {
            return Err(LibraryError::Invalid(
                "max_jobs is required (null for an equal share)".into(),
            ));
        };
        Ok(JobCapRequest {
            cluster_id: self.cluster_id.trim().to_string(),
            max_jobs: max_jobs.map(LinkJobCap::new).transpose()?,
        })
    }
}

/// Parses and validates a `POST /api/config/join/capacity` body from its
/// raw bytes, sans IO, the way the route reads it: JSON into
/// [`CapacityBody`], then [`CapacityBody::validate`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape (unknown or duplicate fields, wrong types, numbers outside `u32`),
/// a missing `max_jobs`, or a cap above [`LinkJobCap::MAX`].
pub fn parse_job_cap_request(body: &[u8]) -> Result<JobCapRequest> {
    serde_json::from_slice::<CapacityBody>(body)
        .map_err(|err| LibraryError::Invalid(format!("capacity body: {err}")))?
        .validate()
}

/// `POST /api/config/join/sharing` body: one library and the settings to
/// change for it.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `SharingFields` enforces it.
#[serde(deny_unknown_fields)]
pub struct SharingBody {
    /// The library (owner cluster id).
    #[schema(example = "6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a")]
    cluster_id: String,
    /// "Share this branch's books with *library*".
    #[schema(example = true)]
    share_books: bool,
    /// "Include books from my own branches".
    #[schema(example = false)]
    include_branches: bool,
    /// "List my members' books to *library*"; absent or `null` keeps the
    /// stored setting.
    #[schema(example = true)]
    list_members_books: Option<bool>,
    /// "Download for other members of *library*": take the library's work
    /// for its other members too, not only this install owner's. Absent or
    /// `null` keeps the stored setting.
    #[schema(example = false)]
    work_for_members: Option<bool>,
    /// "Keep copies of other members' books": also write a book acquired
    /// here for another member to this install's own destinations. Absent
    /// or `null` keeps the stored setting.
    #[schema(example = false)]
    keep_member_copies: Option<bool>,
}

/// [`SharingBody`]'s fields as JSON carries them; read only from a JSON
/// object ([`SharingBody`]'s `Deserialize`), never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharingFields {
    /// See [`SharingBody`].
    cluster_id: String,
    /// See [`SharingBody`].
    share_books: bool,
    /// See [`SharingBody`].
    include_branches: bool,
    /// See [`SharingBody`].
    #[serde(default)]
    list_members_books: Option<bool>,
    /// See [`SharingBody`].
    #[serde(default)]
    work_for_members: Option<bool>,
    /// See [`SharingBody`].
    #[serde(default)]
    keep_member_copies: Option<bool>,
}

impl<'de> Deserialize<'de> for SharingBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: SharingFields = from_object(
            deserializer,
            "a JSON object with cluster_id, share_books and include_branches",
        )?;
        Ok(Self {
            cluster_id: fields.cluster_id,
            share_books: fields.share_books,
            include_branches: fields.include_branches,
            list_members_books: fields.list_members_books,
            work_for_members: fields.work_for_members,
            keep_member_copies: fields.keep_member_copies,
        })
    }
}

/// A sharing settings change: the library it names (trimmed) and the
/// settings to write, each `None` setting left as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharingRequest {
    /// The library (owner cluster id), surrounding whitespace removed.
    pub cluster_id: String,
    /// The settings to write ([`set_cluster_link_settings`]).
    pub settings: ClusterLinkSettings,
}

impl SharingBody {
    /// The change the body asks for.
    #[must_use]
    pub fn into_request(self) -> SharingRequest {
        SharingRequest {
            cluster_id: self.cluster_id.trim().to_string(),
            settings: ClusterLinkSettings {
                share_books: self.share_books,
                include_branches: self.include_branches,
                list_members_books: self.list_members_books,
                work_for_members: self.work_for_members,
                keep_member_copies: self.keep_member_copies,
            },
        }
    }
}

/// Parses a `POST /api/config/join/sharing` body from its raw bytes, sans
/// IO, the way the route reads it: JSON into [`SharingBody`], then
/// [`SharingBody::into_request`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape: not an object, unknown or duplicate fields, a missing
/// `cluster_id`, `share_books` or `include_branches`, or a value of the
/// wrong type.
pub fn parse_sharing_request(body: &[u8]) -> Result<SharingRequest> {
    serde_json::from_slice::<SharingBody>(body)
        .map(SharingBody::into_request)
        .map_err(|err| LibraryError::Invalid(format!("sharing body: {err}")))
}

impl ClusterLink {
    /// True when the host client should connect this link.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == CLUSTER_LINK_ACTIVE
    }

    /// Whose work the branch takes from this library.
    #[must_use]
    pub fn claim_scope(&self) -> bookclerk_node_proto::ClaimScope {
        if self.work_for_members {
            bookclerk_node_proto::ClaimScope::AllMembers
        } else {
            bookclerk_node_proto::ClaimScope::Own
        }
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
            share_books: row.share_books != 0,
            include_branches: row.include_branches != 0,
            ancestry: serde_json::from_str::<Vec<String>>(&row.ancestry_json)
                .unwrap_or_default()
                .into_iter()
                .filter(|id| bookclerk_node_proto::is_cluster_id(id))
                .collect(),
            work_for_members: row.work_for_members != 0,
            keep_member_copies: row.keep_member_copies != 0,
            max_jobs: LinkJobCap::from_column(row.max_jobs),
            list_members_books: row.list_members_books != 0,
            person_user_id: row.person_user_id,
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
/// private keys of a node the owner no longer knows are not kept. The link
/// is stored with no person bound (a re-enrollment unbinds it); the join
/// binds one with [`set_cluster_link_person`].
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
        // A re-enrollment keeps the link's settings and ancestry (the
        // conflict update below leaves them as they are).
        share_books: Set(1),
        include_branches: Set(1),
        ancestry_json: Set("[]".into()),
        work_for_members: Set(0),
        keep_member_copies: Set(0),
        max_jobs: Set(None),
        list_members_books: Set(1),
        // Bound by the join afterwards with [`set_cluster_link_person`]. A
        // re-enrollment unbinds it too (the conflict update below), so a
        // join that cannot name its person leaves the link usable by
        // nobody rather than by whoever joined before.
        person_user_id: Set(None),
    };
    let insert = cluster_links::Entity::insert(row).on_conflict(
        OnConflict::column(cluster_links::Column::OwnerClusterId)
            .update_columns([
                cluster_links::Column::OwnerOrigin,
                cluster_links::Column::RemoteNodeId,
                cluster_links::Column::SpkiPin,
                cluster_links::Column::State,
                cluster_links::Column::EnrolledAt,
                cluster_links::Column::PersonUserId,
            ])
            .to_owned(),
    );
    store
        .exec_bumped(insert, LiveTopic::Clusters, Bump::Always)
        .await?;
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
    let update = cluster_links::Entity::update_many()
        .col_expr(cluster_links::Column::State, Expr::value(state))
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .apply_if(remote_node_id, |query, node| {
            query.filter(cluster_links::Column::RemoteNodeId.eq(node))
        });
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .apply_if(remote_node_id, |query, node| {
            query.filter(cluster_links::Column::RemoteNodeId.eq(node))
        })
        .filter(cluster_links::Column::State.ne(state));
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// Sets a link's two sharing settings: "Share this branch's books with
/// *library*" and "Include books from my own branches". Answers false when
/// no such link exists. The running catalog sync reports them to the owner
/// on its next pass.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_cluster_link_sharing(
    store: &LibraryStore,
    owner_cluster_id: &str,
    share_books: bool,
    include_branches: bool,
) -> Result<bool> {
    let update = cluster_links::Entity::update_many()
        .col_expr(
            cluster_links::Column::ShareBooks,
            Expr::value(i64::from(share_books)),
        )
        .col_expr(
            cluster_links::Column::IncludeBranches,
            Expr::value(i64::from(include_branches)),
        )
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id));
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(
            sea_orm::Condition::any()
                .add(cluster_links::Column::ShareBooks.ne(i64::from(share_books)))
                .add(cluster_links::Column::IncludeBranches.ne(i64::from(include_branches))),
        );
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// Sets the most jobs this install runs for one library at once; `None`
/// clears the cap, leaving the link its equal share of the machine.
/// Answers false when no such link exists. The running client picks the
/// new cap up on the next share update.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_cluster_link_job_cap(
    store: &LibraryStore,
    owner_cluster_id: &str,
    max_jobs: Option<LinkJobCap>,
) -> Result<bool> {
    let value = max_jobs.map(|cap| i64::from(cap.0));
    let update = cluster_links::Entity::update_many()
        .col_expr(cluster_links::Column::MaxJobs, Expr::value(value))
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id));
    let changed = match value {
        Some(value) => sea_orm::Condition::any()
            .add(cluster_links::Column::MaxJobs.is_null())
            .add(cluster_links::Column::MaxJobs.ne(value)),
        None => sea_orm::Condition::all().add(cluster_links::Column::MaxJobs.is_not_null()),
    };
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(changed);
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// Sets the link's "List my members' books to *library*" setting.
///
/// # Arguments
///
/// * `owner_cluster_id` - The library the link joined.
/// * `list_members_books` - The new setting.
///
/// # Returns
///
/// Whether the link exists (`false` when there is no such link); setting
/// the value it already has still returns `true`.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_cluster_link_listing(
    store: &LibraryStore,
    owner_cluster_id: &str,
    list_members_books: bool,
) -> Result<bool> {
    let value = i64::from(list_members_books);
    let update = cluster_links::Entity::update_many()
        .col_expr(cluster_links::Column::ListMembersBooks, Expr::value(value))
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id));
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(cluster_links::Column::ListMembersBooks.ne(value));
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// Binds the link to the local user who joined the library through it.
///
/// Only that person may add their store connections to the library, or
/// withdraw them from it, through this link.
///
/// # Arguments
///
/// * `owner_cluster_id` - The library the link joined.
/// * `remote_node_id` - The node id that join enrolled as: a join another
///   one replaced since binds nothing.
/// * `person_user_id` - The local user who joined (`None` unbinds it).
///
/// # Returns
///
/// Whether the link exists as that node (`false` otherwise).
///
/// # Errors
///
/// Returns the write error.
pub async fn set_cluster_link_person(
    store: &LibraryStore,
    owner_cluster_id: &str,
    remote_node_id: &str,
    person_user_id: Option<i64>,
) -> Result<bool> {
    let update = cluster_links::Entity::update_many()
        .col_expr(
            cluster_links::Column::PersonUserId,
            Expr::value(person_user_id),
        )
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(cluster_links::Column::RemoteNodeId.eq(remote_node_id));
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::Always)
        .await?;
    Ok(updated == 1)
}

/// Sets a link's two work settings: "Download for other members of
/// *library*" and "Keep copies of other members' books". Answers false
/// when no such link exists. The running client claims under the new
/// scope from its next consent check.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_cluster_link_work(
    store: &LibraryStore,
    owner_cluster_id: &str,
    work_for_members: bool,
    keep_member_copies: bool,
) -> Result<bool> {
    let update = cluster_links::Entity::update_many()
        .col_expr(
            cluster_links::Column::WorkForMembers,
            Expr::value(i64::from(work_for_members)),
        )
        .col_expr(
            cluster_links::Column::KeepMemberCopies,
            Expr::value(i64::from(keep_member_copies)),
        )
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id));
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(
            sea_orm::Condition::any()
                .add(cluster_links::Column::WorkForMembers.ne(i64::from(work_for_members)))
                .add(cluster_links::Column::KeepMemberCopies.ne(i64::from(keep_member_copies))),
        );
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// Every setting Settings → Library changes on one link at once; see
/// [`set_cluster_link_settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterLinkSettings {
    /// "Share this branch's books with *library*".
    pub share_books: bool,
    /// "Include books from my own branches".
    pub include_branches: bool,
    /// "List my members' books to *library*"; `None` keeps the stored value.
    pub list_members_books: Option<bool>,
    /// "Download for other members of *library*"; `None` keeps the stored
    /// value.
    pub work_for_members: Option<bool>,
    /// "Keep copies of other members' books"; `None` keeps the stored value.
    pub keep_member_copies: Option<bool>,
}

/// Sets a link's sharing, listing and work settings in one statement, so a
/// failed write leaves every one of them as it was.
///
/// # Arguments
///
/// * `owner_cluster_id` - The library the link joined.
/// * `settings` - The new values; each `None` keeps the stored one.
///
/// # Returns
///
/// Whether the link exists (`false` when there is no such link); setting
/// the values it already has still returns `true`.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_cluster_link_settings(
    store: &LibraryStore,
    owner_cluster_id: &str,
    settings: ClusterLinkSettings,
) -> Result<bool> {
    let optional = [
        (
            cluster_links::Column::ListMembersBooks,
            settings.list_members_books,
        ),
        (
            cluster_links::Column::WorkForMembers,
            settings.work_for_members,
        ),
        (
            cluster_links::Column::KeepMemberCopies,
            settings.keep_member_copies,
        ),
    ];
    let mut update = cluster_links::Entity::update_many()
        .col_expr(
            cluster_links::Column::ShareBooks,
            Expr::value(i64::from(settings.share_books)),
        )
        .col_expr(
            cluster_links::Column::IncludeBranches,
            Expr::value(i64::from(settings.include_branches)),
        )
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id));
    let mut changed = sea_orm::Condition::any()
        .add(cluster_links::Column::ShareBooks.ne(i64::from(settings.share_books)))
        .add(cluster_links::Column::IncludeBranches.ne(i64::from(settings.include_branches)));
    for (column, value) in optional {
        if let Some(value) = value {
            update = update.col_expr(column, Expr::value(i64::from(value)));
            changed = changed.add(column.ne(i64::from(value)));
        }
    }
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(changed);
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// Records the owner library's ancestry as it reported it (`info` at join,
/// `catalog.session` on every sync). Ids that are not plain cluster ids are
/// refused; at most [`bookclerk_node_proto::MAX_ANCESTRY`] are kept.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a malformed ancestry, and the
/// write error.
pub async fn set_cluster_link_ancestry(
    store: &LibraryStore,
    owner_cluster_id: &str,
    ancestry: &[String],
) -> Result<bool> {
    if !bookclerk_node_proto::is_ancestry(ancestry) {
        return Err(LibraryError::Invalid("malformed library ancestry".into()));
    }
    let json = serde_json::to_string(ancestry)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("ancestry encode: {err}")))?;
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(cluster_links::Column::AncestryJson.ne(json.as_str()));
    let update = cluster_links::Entity::update_many()
        .col_expr(cluster_links::Column::AncestryJson, Expr::value(json))
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id));
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
}

/// This library's ancestry (what `GET /api/node/v1/info` reports): its own
/// cluster id, then for each active link the owner's cluster id and the
/// ancestry that owner reported, without repeats, at most
/// [`bookclerk_node_proto::MAX_ANCESTRY`] ids. Empty when this install has
/// no cluster identity yet. A join that would take it past that limit is
/// refused ([`join_overflows_ancestry`]); should a library above grow its
/// ancestry later, the list is cut at exactly the limit, which in turn
/// refuses every join under it.
///
/// A clerk shares its library's database, so it reports the same ancestry:
/// it is the same node of the tree.
///
/// # Errors
///
/// Returns the read error.
pub async fn library_ancestry(store: &LibraryStore) -> Result<Vec<String>> {
    fn push(id: &str, out: &mut Vec<String>) {
        if out.len() < bookclerk_node_proto::MAX_ANCESTRY
            && bookclerk_node_proto::is_cluster_id(id)
            && !out.iter().any(|seen| seen == id)
        {
            out.push(id.to_string());
        }
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(row) = super::load_cluster_row(store).await? {
        push(&row.cluster_id, &mut out);
    }
    for link in list_cluster_links(store).await? {
        if !link.is_active() {
            continue;
        }
        push(&link.owner_cluster_id, &mut out);
        for id in &link.ancestry {
            push(id, &mut out);
        }
    }
    Ok(out)
}

/// True when joining a library that reports `parent_ancestry` would give
/// this library more ancestors (itself and every library above it) than
/// [`bookclerk_node_proto::MAX_ANCESTRY`]: [`library_ancestry`] would then
/// leave the farthest out, and a library joining this one later could no
/// longer be checked for a loop through them. An ancestry cut at that
/// limit by the library itself always holds exactly the limit, so this
/// refuses it too. Such a join is refused rather than cut short.
///
/// # Errors
///
/// Returns the read error.
pub async fn join_overflows_ancestry(
    store: &LibraryStore,
    parent_ancestry: &[String],
) -> Result<bool> {
    let own = library_ancestry(store).await?;
    // An install without a cluster identity yet gets one as it joins.
    let unnamed = usize::from(own.is_empty());
    let ids: std::collections::BTreeSet<&str> = own
        .iter()
        .chain(parent_ancestry)
        .map(String::as_str)
        .collect();
    Ok(ids.len() + unnamed > bookclerk_node_proto::MAX_ANCESTRY)
}

/// This library's own cluster id when `parent_ancestry` names it: joining
/// that library would make a cycle (A ← B ← A, A ← B ← C ← A). `None` when
/// it does not, or this install has no cluster identity yet.
///
/// # Errors
///
/// Returns the read error.
pub async fn join_cycle(
    store: &LibraryStore,
    parent_ancestry: &[String],
) -> Result<Option<String>> {
    let Some(own) = super::load_cluster_row(store).await? else {
        return Ok(None);
    };
    Ok(parent_ancestry
        .contains(&own.cluster_id)
        .then_some(own.cluster_id))
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
    let update = cluster_links::Entity::update_many()
        .col_expr(
            cluster_links::Column::State,
            Expr::value(CLUSTER_LINK_REVOKED),
        )
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(cluster_links::Column::RemoteNodeId.eq(remote_node_id))
        .filter(cluster_links::Column::State.eq(CLUSTER_LINK_ACTIVE));
    let probe = cluster_links::Entity::find()
        .filter(cluster_links::Column::OwnerClusterId.eq(owner_cluster_id))
        .filter(cluster_links::Column::RemoteNodeId.eq(remote_node_id))
        .filter(cluster_links::Column::State.eq(CLUSTER_LINK_ACTIVE));
    let updated = store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    Ok(updated == 1)
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

    /// A job cap is stored, kept across a re-enrollment, cleared, and
    /// refused above the maximum; an unknown cluster answers false. Runs
    /// under every database test profile.
    #[tokio::test]
    async fn a_links_job_cap_round_trips() {
        let _lock = master_key_test_lock_async().await;
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store().clone();
            let dir = tempfile::tempdir().unwrap();
            crate::configure_master_key(dir.path()).expect("master key");
            job_cap_round_trip(&store).await;
            assert_eq!(
                kit.refused_begins(),
                0,
                "{profile:?} took an interactive transaction"
            );
        }
        assert!(LinkJobCap::new(u32::from(LinkJobCap::MAX) + 1).is_err());
        assert!(LinkJobCap::new(u32::from(LinkJobCap::MAX)).is_ok());
    }

    /// Writes, keeps, clears and misses a job cap on `store`.
    async fn job_cap_round_trip(store: &LibraryStore) {
        let store = store.clone();
        let keys = ClusterLinkKeys::from_bytes(&[3; 32], &[4; 32]);
        record_cluster_link(&store, &new_link(None), &keys)
            .await
            .unwrap();
        let read = |store: LibraryStore| async move {
            load_cluster_link(&store, "cluster-1")
                .await
                .unwrap()
                .expect("the link")
                .max_jobs
        };
        assert_eq!(read(store.clone()).await, None);
        let cap = LinkJobCap::new(3).unwrap();
        assert!(set_cluster_link_job_cap(&store, "cluster-1", Some(cap))
            .await
            .unwrap());
        assert_eq!(read(store.clone()).await, Some(cap));
        assert_eq!(cap.jobs(), 3);
        record_cluster_link(&store, &new_link(None), &keys)
            .await
            .unwrap();
        assert_eq!(
            read(store.clone()).await,
            Some(cap),
            "re-enrollment dropped the cap"
        );
        let none = LinkJobCap::new(0).unwrap();
        assert!(set_cluster_link_job_cap(&store, "cluster-1", Some(none))
            .await
            .unwrap());
        assert_eq!(read(store.clone()).await, Some(none));
        assert!(set_cluster_link_job_cap(&store, "cluster-1", None)
            .await
            .unwrap());
        assert_eq!(read(store.clone()).await, None);
        assert!(!set_cluster_link_job_cap(&store, "nope", Some(cap))
            .await
            .unwrap());
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

        // Re-enrolling replaces the link and its keys and reactivates it,
        // and unbinds the person until the join names one again.
        assert!(
            set_cluster_link_person(&store, "cluster-1", "node-1", Some(7))
                .await
                .unwrap()
        );
        // A join that enrolled as another node binds nothing.
        assert!(
            !set_cluster_link_person(&store, "cluster-1", "node-0", Some(8))
                .await
                .unwrap()
        );
        let fresh = ClusterLinkKeys::from_bytes(&[3; 32], &[4; 32]);
        let again = record_cluster_link(&store, &new_link(None), &fresh)
            .await
            .unwrap();
        assert!(again.is_active());
        assert_eq!(again.spki_pin, None);
        assert_eq!(again.person_user_id, None);
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
