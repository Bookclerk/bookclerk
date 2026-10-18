//! Host-node enrollment: single-use links, redeem, and revocation.
//!
//! A link enrolls one independent Bookclerk install (a *host node*) into this
//! cluster. The owner keeps only `hash_token(secret)`; the secret travels in
//! the URL fragment of `https://<origin>/enroll#bcn1.<link_id>.<secret>`,
//! which proxies and access logs never see.
//!
//! Who may mint (decision 5 of the H-4 design):
//!
//! - nobody, while the cluster's `core.host_enrollment` switch is off;
//! - the operator, or an active Owner, for any active user;
//! - any other active user, for themselves only.
//!
//! Redeeming checks the hash, expiry, single use, revocation, the link
//! user's status, and the enrollment switch inside one canonical batch that
//! also inserts the `nodes` row (`kind = host`, owned by the link's user),
//! the `node_credentials` row, and an idempotency receipt. Two redeems of one
//! link create at most one node on every backend.
//!
//! Owning a node does not make the node trusted: work still follows the
//! owner's role (see `docs/adr/control-plane.md`).

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rand::RngCore;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::batch::{self, exec, int, query, request, text};
use super::documents::{
    import_if_absent, load_document, replace_document, ConfigActor, DocumentKey, ReplaceOutcome,
    StoredDocument, VerifiedClusterOwner, CLUSTER_SCOPE_ID, CLUSTER_SCOPE_TYPE,
};
use super::node_queue::{notify_all_nodes, NodeMessage};
use super::{document, ConfigurationDocument};
use crate::entities::{bookclerk_receipts, node_credentials, node_enrollment_links};
use crate::error::{LibraryError, Result};
use crate::models::{UserRecord, UserRole, UserStatus};
use crate::store::LibraryStore;
use crate::token_hash::hash_token;

/// Namespace of the cluster's host-enrollment switch.
pub const HOST_ENROLLMENT_NAMESPACE: &str = "core.host_enrollment";

/// Schema version of [`HostEnrollmentSettingsV1`].
pub const HOST_ENROLLMENT_SCHEMA_VERSION: i64 = 1;

/// Version tag that starts every enrollment token.
pub const ENROLLMENT_TOKEN_PREFIX: &str = "bcn1";

/// Default lifetime of a link.
pub const DEFAULT_LINK_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Longest lifetime a link may be minted with.
pub const MAX_LINK_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Shortest lifetime a link may be minted with.
pub const MIN_LINK_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Size of an Ed25519 or X25519 public key.
pub const NODE_PUBLIC_KEY_LEN: usize = 32;

/// Longest `label` stored on a link.
pub const MAX_LINK_LABEL_BYTES: usize = 64;

/// Longest `software_version` a redeem may report.
const MAX_SOFTWARE_VERSION_BYTES: usize = 64;

/// Random bytes in a link secret.
const SECRET_BYTES: usize = 32;

/// `bookclerk_receipts.operation_kind` of a redeem.
const REDEEM_KIND: &str = "enrollNode";

/// How long a redeem receipt answers a retry.
const REDEEM_RECEIPT_TTL_DAYS: i64 = 7;

/// Same refusal for every failed redeem, so a caller cannot tell an unknown
/// link from a used, expired, or revoked one.
fn redeem_refused() -> LibraryError {
    LibraryError::Forbidden("enrollment link is invalid, expired, revoked, or already used".into())
}

/// Refusal while the cluster's switch is off.
fn enrollment_disabled() -> LibraryError {
    LibraryError::Forbidden("host enrollment is disabled for this cluster".into())
}

/// Fixed-width UTC timestamp, so stored times compare correctly as text.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Typed `core.host_enrollment` document, schema version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEnrollmentSettingsV1 {
    /// When false, no link may be minted or redeemed.
    pub enabled: bool,
}

impl Default for HostEnrollmentSettingsV1 {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The host-enrollment switch as stored, or its default when no document
/// was ever written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEnrollmentSettings {
    /// Effective settings.
    pub body: HostEnrollmentSettingsV1,
    /// Stored revision; `None` while the default applies.
    pub revision: Option<i64>,
}

/// Reads the cluster's host-enrollment switch. A missing document means the
/// default: enabled.
///
/// # Errors
///
/// Returns an error when the read fails or the stored document has an
/// unsupported schema or an invalid body.
pub async fn load_host_enrollment(store: &LibraryStore) -> Result<HostEnrollmentSettings> {
    match load_document(store, &DocumentKey::cluster(HOST_ENROLLMENT_NAMESPACE)).await? {
        None => Ok(HostEnrollmentSettings {
            body: HostEnrollmentSettingsV1::default(),
            revision: None,
        }),
        Some(stored) => {
            let doc = parse_host_enrollment(stored)?;
            Ok(HostEnrollmentSettings {
                body: doc.body,
                revision: Some(doc.revision),
            })
        }
    }
}

/// Turns host enrollment on or off for the cluster.
///
/// Only the operator or a verified Owner ([`verify_cluster_owner`]) may write
/// it. Retries a few times on a concurrent edit.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] for any other actor, and the write
/// error when the document cannot be stored.
pub async fn set_host_enrollment_enabled(
    store: &LibraryStore,
    actor: &ConfigActor,
    enabled: bool,
) -> Result<ConfigurationDocument<HostEnrollmentSettingsV1>> {
    if !matches!(
        actor,
        ConfigActor::Operator { .. } | ConfigActor::ClusterOwner(_)
    ) {
        return Err(LibraryError::Forbidden(
            "only the operator or an Owner may change host enrollment".into(),
        ));
    }
    let key = DocumentKey::cluster(HOST_ENROLLMENT_NAMESPACE);
    let body = HostEnrollmentSettingsV1 { enabled };
    let json = serde_json::to_string(&body)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    for attempt in 0..4 {
        let operation_id = format!(
            "host-enrollment-{attempt}-{}",
            uuid::Uuid::new_v4().simple()
        );
        let Some(current) = load_document(store, &key).await? else {
            let stored = import_if_absent(
                store,
                actor,
                &key,
                HOST_ENROLLMENT_SCHEMA_VERSION,
                &json,
                &operation_id,
            )
            .await?;
            let doc = parse_host_enrollment(stored)?;
            if doc.body == body {
                return Ok(doc);
            }
            // A concurrent writer created it first; replace on the next pass.
            continue;
        };
        let doc = parse_host_enrollment(current)?;
        if doc.body == body {
            return Ok(doc);
        }
        match replace_document(
            store,
            actor,
            &key,
            HOST_ENROLLMENT_SCHEMA_VERSION,
            doc.revision,
            &json,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse_host_enrollment(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(
        "core.host_enrollment kept changing; retry".into(),
    ))
}

/// Parses a stored `core.host_enrollment` row, failing closed on an unknown
/// schema.
fn parse_host_enrollment(
    stored: StoredDocument,
) -> Result<ConfigurationDocument<HostEnrollmentSettingsV1>> {
    if stored.schema_version != HOST_ENROLLMENT_SCHEMA_VERSION {
        return Err(super::documents::unsupported(
            HOST_ENROLLMENT_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: HostEnrollmentSettingsV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    Ok(document(stored, body))
}

/// Returns a [`ConfigActor::ClusterOwner`] when `user_id` is an active Owner.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] for anyone else (including an unknown
/// user), and the read error when the lookup fails.
pub async fn verify_cluster_owner(store: &LibraryStore, user_id: i64) -> Result<ConfigActor> {
    match store.get_user(user_id).await? {
        Some(user) if user.status == UserStatus::Active && user.role == UserRole::Owner => {
            Ok(ConfigActor::ClusterOwner(VerifiedClusterOwner {
                user_id,
                audit: format!("owner:{user_id}"),
            }))
        }
        _ => Err(LibraryError::Forbidden(format!(
            "user {user_id} is not an active owner"
        ))),
    }
}

/// Who is acting on enrollment links and node credentials.
///
/// A `User` is resolved from the database on every call: the role and
/// status the library reads, not what the caller claims, decide what it may
/// do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollmentActor {
    /// Control-plane operator (CLI or operator token).
    Operator {
        /// Stable audit id (`cli`, `operator`, …).
        id: String,
    },
    /// A signed-in portal user.
    User {
        /// Calling user.
        user_id: i64,
    },
}

/// What an [`EnrollmentActor`] resolved to.
enum Resolved {
    /// Operator, or an active Owner: acts on every link and node.
    Admin {
        /// Audit id.
        audit: String,
    },
    /// Any other active user: acts on their own links and nodes.
    SelfOnly {
        /// The user.
        user_id: i64,
    },
}

impl Resolved {
    /// Audit id recorded on a minted link.
    fn audit(&self) -> String {
        match self {
            Self::Admin { audit } => audit.clone(),
            Self::SelfOnly { user_id } => format!("user:{user_id}"),
        }
    }

    /// True when this actor may act for `user_id`.
    fn covers_user(&self, user_id: i64) -> bool {
        match self {
            Self::Admin { .. } => true,
            Self::SelfOnly { user_id: me } => *me == user_id,
        }
    }
}

/// Resolves `actor` against the stored user row; a disabled or unknown user
/// is refused.
async fn resolve(store: &LibraryStore, actor: &EnrollmentActor) -> Result<Resolved> {
    match actor {
        EnrollmentActor::Operator { id } => Ok(Resolved::Admin {
            audit: format!("operator:{id}"),
        }),
        EnrollmentActor::User { user_id } => {
            let user = active_user(store, *user_id).await.map_err(|_| {
                LibraryError::Forbidden(format!("user {user_id} is not an active user"))
            })?;
            if user.role == UserRole::Owner {
                Ok(Resolved::Admin {
                    audit: format!("owner:{user_id}"),
                })
            } else {
                Ok(Resolved::SelfOnly { user_id: *user_id })
            }
        }
    }
}

/// The active user `user_id`.
async fn active_user(store: &LibraryStore, user_id: i64) -> Result<UserRecord> {
    let user = store
        .get_user(user_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("user {user_id}")))?;
    if user.status != UserStatus::Active {
        return Err(LibraryError::Invalid(format!("user {user_id} is disabled")));
    }
    Ok(user)
}

/// A link's plaintext secret. Shown once at mint and never stored; `Debug`
/// does not print it.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollmentSecret(Zeroizing<String>);

impl EnrollmentSecret {
    /// Wraps a secret a host submitted.
    #[must_use]
    pub fn from_text(secret: String) -> Self {
        Self(Zeroizing::new(secret))
    }

    /// The secret text (base64url, no padding).
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for EnrollmentSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnrollmentSecret(***)")
    }
}

/// One link as listings show it. The secret and its hash are not included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnrollmentLink {
    /// Public link id.
    pub link_id: String,
    /// User the enrolled node will belong to.
    pub for_user_id: i64,
    /// Free-form label.
    pub label: String,
    /// Audit id of the minter.
    pub created_by: String,
    /// RFC 3339 mint time.
    pub created_at: String,
    /// RFC 3339 expiry.
    pub expires_at: String,
    /// RFC 3339 redeem time.
    pub redeemed_at: Option<String>,
    /// Node the redeem created.
    pub redeemed_node_id: Option<String>,
    /// RFC 3339 revoke time.
    pub revoked_at: Option<String>,
}

impl EnrollmentLink {
    /// `pending`, `redeemed`, `revoked`, or `expired` at `now`.
    #[must_use]
    pub fn state(&self, now: DateTime<Utc>) -> &'static str {
        if self.redeemed_at.is_some() {
            "redeemed"
        } else if self.revoked_at.is_some() {
            "revoked"
        } else if self.expires_at <= stamp(now) {
            "expired"
        } else {
            "pending"
        }
    }
}

impl From<node_enrollment_links::Model> for EnrollmentLink {
    fn from(row: node_enrollment_links::Model) -> Self {
        Self {
            link_id: row.link_id,
            for_user_id: row.for_user_id,
            label: row.label,
            created_by: row.created_by,
            created_at: row.created_at,
            expires_at: row.expires_at,
            redeemed_at: row.redeemed_at,
            redeemed_node_id: row.redeemed_node_id,
            revoked_at: row.revoked_at,
        }
    }
}

/// A freshly minted link and its secret, which is returned only here.
#[derive(Debug, Clone)]
pub struct MintedLink {
    /// Stored link.
    pub link: EnrollmentLink,
    /// Plaintext secret; put it in the URL fragment and drop it.
    pub secret: EnrollmentSecret,
}

impl MintedLink {
    /// The `bcn1.<link_id>.<secret>[.<spki_pin>]` token.
    #[must_use]
    pub fn token(&self, spki_pin: Option<&str>) -> Zeroizing<String> {
        format_enrollment_token(&self.link.link_id, &self.secret, spki_pin)
    }

    /// `<origin>/enroll#<token>`: the secret stays in the fragment.
    #[must_use]
    pub fn url(&self, origin: &str, spki_pin: Option<&str>) -> Zeroizing<String> {
        let token = self.token(spki_pin);
        Zeroizing::new(format!(
            "{}/enroll#{}",
            origin.trim_end_matches('/'),
            token.as_str()
        ))
    }
}

/// Formats `bcn1.<link_id>.<secret>[.<spki_pin>]`.
#[must_use]
pub fn format_enrollment_token(
    link_id: &str,
    secret: &EnrollmentSecret,
    spki_pin: Option<&str>,
) -> Zeroizing<String> {
    let mut token = format!("{ENROLLMENT_TOKEN_PREFIX}.{link_id}.{}", secret.expose());
    if let Some(pin) = spki_pin.filter(|pin| !pin.is_empty()) {
        token.push('.');
        token.push_str(pin);
    }
    Zeroizing::new(token)
}

/// A parsed enrollment token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentToken {
    /// Link id.
    pub link_id: String,
    /// Link secret.
    pub secret: EnrollmentSecret,
    /// SPKI pin of the owner's self-signed certificate, when present.
    pub spki_pin: Option<String>,
}

/// Parses a token, or a whole link whose fragment is the token.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] when the text is not a `bcn1` token.
pub fn parse_enrollment_token(raw: &str) -> Result<EnrollmentToken> {
    let raw = raw.trim();
    let token = raw.rsplit_once('#').map_or(raw, |(_, fragment)| fragment);
    let invalid = || LibraryError::Invalid("not a bcn1 enrollment link".into());
    let mut parts = token.split('.');
    if parts.next() != Some(ENROLLMENT_TOKEN_PREFIX) {
        return Err(invalid());
    }
    let link_id = parts
        .next()
        .filter(|id| is_link_id(id))
        .ok_or_else(invalid)?;
    let secret = parts
        .next()
        .filter(|secret| is_base64url(secret) && !secret.is_empty())
        .ok_or_else(invalid)?;
    let spki_pin = match parts.next() {
        Some(pin) if is_base64url(pin) && !pin.is_empty() => Some(pin.to_string()),
        Some(_) => return Err(invalid()),
        None => None,
    };
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok(EnrollmentToken {
        link_id: link_id.to_string(),
        secret: EnrollmentSecret(Zeroizing::new(secret.to_string())),
        spki_pin,
    })
}

/// A minted link id is 32 lowercase hex characters.
fn is_link_id(raw: &str) -> bool {
    raw.len() == 32
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// True for base64url text without padding.
fn is_base64url(raw: &str) -> bool {
    raw.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Mints a link that enrolls one host node for `for_user_id`.
///
/// `ttl` defaults to 24 hours and must be 5 minutes to 7 days.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] when enrollment is disabled, the
/// actor is disabled, or a non-Owner mints for someone else;
/// [`LibraryError::NotFound`] / [`LibraryError::Invalid`] when the target
/// user is unknown or disabled or the ttl or label is out of range; and the
/// write error when the insert fails.
pub async fn mint_enrollment_link(
    store: &LibraryStore,
    actor: &EnrollmentActor,
    for_user_id: i64,
    label: &str,
    ttl: Option<std::time::Duration>,
) -> Result<MintedLink> {
    let resolved = resolve(store, actor).await?;
    // Authorize before reading the target, so a member cannot probe user ids.
    if !resolved.covers_user(for_user_id) {
        return Err(LibraryError::Forbidden(
            "only the operator or an Owner may mint an enrollment link for another user".into(),
        ));
    }
    if !load_host_enrollment(store).await?.body.enabled {
        return Err(enrollment_disabled());
    }
    let ttl = ttl.unwrap_or(DEFAULT_LINK_TTL);
    if !(MIN_LINK_TTL..=MAX_LINK_TTL).contains(&ttl) {
        return Err(LibraryError::Invalid(
            "enrollment link ttl must be between 5 minutes and 7 days".into(),
        ));
    }
    let label = label.trim();
    if label.len() > MAX_LINK_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(LibraryError::Invalid(format!(
            "enrollment link label must be at most {MAX_LINK_LABEL_BYTES} bytes without control characters"
        )));
    }
    active_user(store, for_user_id).await?;

    let mut raw = Zeroizing::new([0u8; SECRET_BYTES]);
    rand::rngs::OsRng.fill_bytes(raw.as_mut());
    let secret = EnrollmentSecret(Zeroizing::new(base64_url(raw.as_ref())));
    let link_id = uuid::Uuid::new_v4().simple().to_string();
    let now = Utc::now();
    let ttl = Duration::from_std(ttl)
        .map_err(|_| LibraryError::Invalid("enrollment link ttl is out of range".into()))?;
    let model = node_enrollment_links::ActiveModel {
        link_id: Set(link_id.clone()),
        token_hash: Set(hash_token(secret.expose())),
        for_user_id: Set(for_user_id),
        label: Set(label.to_string()),
        created_by: Set(resolved.audit()),
        created_at: Set(stamp(now)),
        expires_at: Set(stamp(now + ttl)),
        redeemed_at: Set(None),
        redeemed_node_id: Set(None),
        revoked_at: Set(None),
    };
    node_enrollment_links::Entity::insert(model)
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let link = load_link(store, &link_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("enrollment link {link_id}")))?;
    tracing::info!(
        link_id = %link.link_id,
        for_user_id,
        created_by = %link.created_by,
        expires_at = %link.expires_at,
        "minted host enrollment link"
    );
    Ok(MintedLink { link, secret })
}

/// base64url without padding.
fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// One stored link.
async fn load_link(store: &LibraryStore, link_id: &str) -> Result<Option<EnrollmentLink>> {
    Ok(
        node_enrollment_links::Entity::find_by_id(link_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(EnrollmentLink::from),
    )
}

/// Links `actor` may see, newest first: every link for the operator or an
/// Owner, the caller's own links for anyone else.
///
/// # Errors
///
/// Returns an error when the actor is not an active user or the read fails.
pub async fn list_enrollment_links(
    store: &LibraryStore,
    actor: &EnrollmentActor,
) -> Result<Vec<EnrollmentLink>> {
    let resolved = resolve(store, actor).await?;
    let mut find = node_enrollment_links::Entity::find()
        .order_by_desc(node_enrollment_links::Column::CreatedAt)
        .order_by_asc(node_enrollment_links::Column::LinkId);
    if let Resolved::SelfOnly { user_id } = resolved {
        find = find.filter(node_enrollment_links::Column::ForUserId.eq(user_id));
    }
    Ok(find
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(EnrollmentLink::from)
        .collect())
}

/// Revokes an unredeemed link. Returns the link as stored afterwards.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] when the link is unknown or not the
/// caller's (the two read the same), [`LibraryError::Conflict`] when it was
/// already redeemed, and the write error when the update fails.
pub async fn revoke_enrollment_link(
    store: &LibraryStore,
    actor: &EnrollmentActor,
    link_id: &str,
) -> Result<EnrollmentLink> {
    let resolved = resolve(store, actor).await?;
    let not_found = || LibraryError::NotFound(format!("enrollment link {link_id}"));
    let link = load_link(store, link_id).await?.ok_or_else(not_found)?;
    if !resolved.covers_user(link.for_user_id) {
        return Err(not_found());
    }
    if link.redeemed_at.is_some() {
        return Err(LibraryError::Conflict(format!(
            "enrollment link {link_id} was already redeemed; revoke the node instead"
        )));
    }
    node_enrollment_links::Entity::update_many()
        .col_expr(
            node_enrollment_links::Column::RevokedAt,
            Expr::value(stamp(Utc::now())),
        )
        .filter(node_enrollment_links::Column::LinkId.eq(link_id))
        .filter(node_enrollment_links::Column::RevokedAt.is_null())
        .filter(node_enrollment_links::Column::RedeemedAt.is_null())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let link = load_link(store, link_id).await?.ok_or_else(not_found)?;
    if link.redeemed_at.is_some() && link.revoked_at.is_none() {
        return Err(LibraryError::Conflict(format!(
            "enrollment link {link_id} was redeemed before it could be revoked"
        )));
    }
    tracing::info!(link_id, "revoked host enrollment link");
    Ok(link)
}

/// Prefix of the `created_by` audit id of a link a sign-in created.
pub const AUTHORIZED_LINK_CREATED_BY: &str = "oauth:";

/// Lifetime of a link a sign-in created: it is redeemed in the same
/// request, so it only has to outlive a retry.
const AUTHORIZED_LINK_TTL: Duration = Duration::minutes(10);

/// The link a branch sign-in redeems through: its id and secret are derived
/// from the authorization code and the PKCE verifier, so a retry of the
/// same exchange (same code, verifier and keys) finds the same link and
/// answers from its receipt, and a wrong verifier can never redeem it.
///
/// The id is `hex(SHA-256("bookclerk-branch-link-v1\0" || code))[..32]`; the
/// secret is base64url `SHA-256("bookclerk-branch-secret-v1\0" || code ||
/// "\0" || verifier)`.
#[must_use]
pub fn authorized_enrollment_link(code: &str, verifier: &str) -> (String, EnrollmentSecret) {
    let mut id = Sha256::new();
    id.update(b"bookclerk-branch-link-v1\0");
    id.update(code.as_bytes());
    let link_id = hex::encode(id.finalize())[..32].to_string();
    let mut secret = Sha256::new();
    secret.update(b"bookclerk-branch-secret-v1\0");
    secret.update(code.as_bytes());
    secret.update([0]);
    secret.update(verifier.as_bytes());
    let secret = Zeroizing::new(secret.finalize().to_vec());
    (
        link_id,
        EnrollmentSecret(Zeroizing::new(base64_url(secret.as_slice()))),
    )
}

/// Records the link an approved branch sign-in redeems through
/// ([`authorized_enrollment_link`]), for `for_user_id`, the user who signed
/// in and consented. Called only after the authorization code was consumed
/// and its PKCE verifier checked; the redeem itself is
/// [`redeem_enrollment_link`], the same canonical batch a minted link takes.
///
/// A second call for the same link id leaves the first row as it is.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] while host enrollment is off or the
/// user is not active, [`LibraryError::Invalid`] for a malformed label, and
/// the write error.
pub async fn record_authorized_enrollment_link(
    store: &LibraryStore,
    link_id: &str,
    secret: &EnrollmentSecret,
    for_user_id: i64,
    label: &str,
) -> Result<()> {
    if !is_link_id(link_id) {
        return Err(LibraryError::Invalid("malformed link id".into()));
    }
    if !load_host_enrollment(store).await?.body.enabled {
        return Err(enrollment_disabled());
    }
    active_user(store, for_user_id)
        .await
        .map_err(|_| redeem_refused())?;
    let label = label.trim();
    if label.len() > MAX_LINK_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(LibraryError::Invalid(format!(
            "branch name must be at most {MAX_LINK_LABEL_BYTES} bytes without control characters"
        )));
    }
    let now = Utc::now();
    let model = node_enrollment_links::ActiveModel {
        link_id: Set(link_id.to_string()),
        token_hash: Set(hash_token(secret.expose())),
        for_user_id: Set(for_user_id),
        label: Set(label.to_string()),
        created_by: Set(format!("{AUTHORIZED_LINK_CREATED_BY}user:{for_user_id}")),
        created_at: Set(stamp(now)),
        expires_at: Set(stamp(now + AUTHORIZED_LINK_TTL)),
        redeemed_at: Set(None),
        redeemed_node_id: Set(None),
        revoked_at: Set(None),
    };
    node_enrollment_links::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(node_enrollment_links::Column::LinkId)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// A host announced it is leaving (`node.leaving`): revokes its credential
/// at once and drops its live status, so it stops counting as a node
/// without waiting for missed heartbeats, and tells every node so the
/// connection is closed wherever it is held. Answers false when the node
/// has no unrevoked credential.
///
/// # Errors
///
/// Returns the write error.
pub async fn record_node_left(store: &LibraryStore, node_id: &str) -> Result<bool> {
    let updated = node_credentials::Entity::update_many()
        .col_expr(
            node_credentials::Column::RevokedAt,
            Expr::value(stamp(Utc::now())),
        )
        .filter(node_credentials::Column::NodeId.eq(node_id))
        .filter(node_credentials::Column::RevokedAt.is_null())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if updated.rows_affected != 1 {
        return Ok(false);
    }
    crate::entities::node_status::Entity::delete_many()
        .filter(crate::entities::node_status::Column::NodeId.eq(node_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    // Titles it shared stop being shared; their files stay here.
    super::branch_catalog::detach_branch_titles(store, node_id, super::DETACHED_LEFT).await?;
    let _ = store
        .insert_security_audit_event(&format!("node:{node_id}"), "node_left", None)
        .await;
    tracing::info!(node_id, "host node left the cluster");
    notify_all_nodes(
        store,
        NodeMessage::NodeRevoked {
            node_id: node_id.to_string(),
        },
    )
    .await;
    Ok(true)
}

/// What a host submits to redeem a link.
#[derive(Clone, PartialEq, Eq)]
pub struct RedeemRequest {
    /// Link id from the token.
    pub link_id: String,
    /// Link secret from the token.
    pub secret: EnrollmentSecret,
    /// Ed25519 public signing key (32 bytes).
    pub sign_public_key: Vec<u8>,
    /// X25519 public box key (32 bytes).
    pub box_public_key: Vec<u8>,
    /// Bookclerk version of the host.
    pub software_version: String,
}

impl std::fmt::Debug for RedeemRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedeemRequest")
            .field("link_id", &self.link_id)
            .field("secret", &self.secret)
            .field("software_version", &self.software_version)
            .finish_non_exhaustive()
    }
}

/// Result of a successful redeem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnrolledNode {
    /// Owner cluster id.
    pub cluster_id: String,
    /// Node id minted for the host.
    pub node_id: String,
    /// User who owns the node.
    pub owner_user_id: i64,
    /// True when this answered a retry of an earlier successful redeem.
    pub replayed: bool,
}

/// Receipt payload of a redeem.
#[derive(Debug, Serialize, Deserialize)]
struct RedeemReceipt {
    /// Owner cluster id.
    cluster_id: String,
    /// Node id minted for the host.
    node_id: String,
    /// Owning user.
    owner_user_id: i64,
}

/// Rejects a signing or box key that is not a canonical prime-order
/// Curve25519 point of [`NODE_PUBLIC_KEY_LEN`] bytes.
fn validate_public_keys(sign_public_key: &[u8], box_public_key: &[u8]) -> Result<()> {
    bookclerk_node_proto::validate_sign_public_key(sign_public_key)
        .map_err(|err| LibraryError::Invalid(format!("sign_pk: {err}")))?;
    bookclerk_node_proto::validate_box_public_key(box_public_key)
        .map_err(|err| LibraryError::Invalid(format!("box_pk: {err}")))?;
    Ok(())
}

/// Redeems a link: one canonical batch marks it redeemed, inserts a host
/// `nodes` row owned by the link's user, inserts its `node_credentials`,
/// and records a receipt.
///
/// The batch's predicates re-check the link hash, expiry, single use,
/// revocation, the user's status, and that the enrollment switch is still
/// the revision read as enabled, so a concurrent disable or second redeem
/// fails closed. A retry with the same link, secret, and keys after a lost
/// reply returns the same node (`replayed`).
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for malformed keys,
/// [`LibraryError::Forbidden`] when enrollment is disabled or the link does
/// not redeem (one message for every reason), and the write error when the
/// batch fails.
pub async fn redeem_enrollment_link(
    store: &LibraryStore,
    req: &RedeemRequest,
) -> Result<EnrolledNode> {
    validate_public_keys(&req.sign_public_key, &req.box_public_key)?;
    let software_version = req.software_version.trim();
    if software_version.len() > MAX_SOFTWARE_VERSION_BYTES
        || software_version.chars().any(char::is_control)
    {
        return Err(LibraryError::Invalid(
            "software_version must be at most 64 bytes without control characters".into(),
        ));
    }
    if !is_link_id(&req.link_id) || !is_base64url(req.secret.expose()) {
        return Err(redeem_refused());
    }
    let token_hash = hash_token(req.secret.expose());
    let operation_id = format!("{REDEEM_KIND}:{}", req.link_id);
    let request_hash = redeem_hash(req, &token_hash);
    if let Some(done) = redeem_replay(store, &operation_id, &request_hash).await? {
        return Ok(done);
    }
    let enrollment = load_host_enrollment(store).await?;
    if !enrollment.body.enabled {
        return Err(enrollment_disabled());
    }
    let cluster_id = super::load_cluster_row(store)
        .await?
        .ok_or_else(|| LibraryError::Unavailable("cluster identity is not initialized".into()))?
        .cluster_id;
    // The link's user decides the owner; read it for the receipt. The batch
    // re-checks everything this read implies.
    let Some(link) = load_link(store, &req.link_id).await? else {
        return Err(redeem_refused());
    };
    let node_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();
    let now_text = stamp(now);
    let receipt = serde_json::to_string(&RedeemReceipt {
        cluster_id: cluster_id.clone(),
        node_id: node_id.clone(),
        owner_user_id: link.for_user_id,
    })
    .map_err(|err| LibraryError::Other(anyhow::anyhow!("redeem receipt encode: {err}")))?;
    let (switch_sql, mut switch_binds) = match enrollment.revision {
        None => (
            "NOT EXISTS (SELECT 1 FROM configuration_documents \
              WHERE scope_type = ? AND scope_id = ? AND namespace = ?)",
            Vec::new(),
        ),
        Some(revision) => (
            "EXISTS (SELECT 1 FROM configuration_documents \
              WHERE scope_type = ? AND scope_id = ? AND namespace = ? AND revision = ?)",
            vec![int(revision)],
        ),
    };
    let mut claim_binds = vec![
        text(&now_text),
        text(&node_id),
        text(&req.link_id),
        text(&token_hash),
        text(&now_text),
        int(link.for_user_id),
        text(CLUSTER_SCOPE_TYPE),
        text(CLUSTER_SCOPE_ID),
        text(HOST_ENROLLMENT_NAMESPACE),
    ];
    claim_binds.append(&mut switch_binds);
    let claim_sql = format!(
        "UPDATE node_enrollment_links SET redeemed_at = ?, redeemed_node_id = ?
          WHERE link_id = ? AND token_hash = ?
            AND redeemed_at IS NULL AND revoked_at IS NULL AND expires_at > ?
            AND for_user_id = ?
            AND for_user_id IN (SELECT id FROM users WHERE status = 'active')
            AND {switch_sql}"
    );
    let expires = stamp(now + Duration::days(REDEEM_RECEIPT_TTL_DAYS));
    let reply = batch::execute_host_batch(
        store,
        request(
            &operation_id,
            vec![
                exec(&claim_sql, claim_binds),
                exec(
                    "INSERT INTO nodes (
                        node_id, cluster_id, created_at, incarnation, heartbeat_at,
                        software_version, schema_state, compatible, target, labels_json,
                        kind, owner_user_id
                     ) SELECT ?, ?, ?, '', ?, ?, '', 0, '', '[]', 'host', for_user_id
                         FROM node_enrollment_links
                        WHERE link_id = ? AND redeemed_node_id = ?",
                    vec![
                        text(&node_id),
                        text(&cluster_id),
                        text(&now_text),
                        text(&now_text),
                        text(software_version),
                        text(&req.link_id),
                        text(&node_id),
                    ],
                ),
                exec(
                    "INSERT INTO node_credentials (
                        node_id, sign_public_key, box_public_key, link_id, enrolled_at
                     ) SELECT ?, ?, ?, link_id, ?
                         FROM node_enrollment_links
                        WHERE link_id = ? AND redeemed_node_id = ?",
                    vec![
                        text(&node_id),
                        bookclerk_plugin_abi::DbValue::Bytes(req.sign_public_key.clone()),
                        bookclerk_plugin_abi::DbValue::Bytes(req.box_public_key.clone()),
                        text(&now_text),
                        text(&req.link_id),
                        text(&node_id),
                    ],
                ),
                exec(
                    "INSERT INTO bookclerk_receipts (
                        operation_id, operation_kind, request_hash, status, payload,
                        created_at, expires_at, consume_key
                     ) SELECT ?, ?, ?, 'ok', ?, ?, ?, NULL
                         FROM node_enrollment_links
                        WHERE link_id = ? AND redeemed_node_id = ?",
                    vec![
                        text(&operation_id),
                        text(REDEEM_KIND),
                        text(&request_hash),
                        text(&receipt),
                        text(&now_text),
                        text(&expires),
                        text(&req.link_id),
                        text(&node_id),
                    ],
                ),
                query(
                    "SELECT redeemed_node_id FROM node_enrollment_links WHERE link_id = ?",
                    vec![text(&req.link_id)],
                    1,
                ),
            ],
        ),
    )
    .await?;
    if batch::rows_affected(&reply, 0) == 1 {
        tracing::info!(
            link_id = %req.link_id,
            node_id = %node_id,
            owner_user_id = link.for_user_id,
            "host node enrolled"
        );
        return Ok(EnrolledNode {
            cluster_id,
            node_id,
            owner_user_id: link.for_user_id,
            replayed: false,
        });
    }
    // A concurrent identical retry may have committed in between.
    if let Some(done) = redeem_replay(store, &operation_id, &request_hash).await? {
        return Ok(done);
    }
    tracing::warn!(link_id = %req.link_id, "host enrollment redeem refused");
    Err(redeem_refused())
}

/// Request hash of a redeem: the link, the secret's hash, and both keys.
fn redeem_hash(req: &RedeemRequest, token_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bookclerk-enroll-node-v1\0");
    hasher.update(req.link_id.as_bytes());
    hasher.update([0]);
    hasher.update(token_hash.as_bytes());
    hasher.update([0]);
    hasher.update(&req.sign_public_key);
    hasher.update(&req.box_public_key);
    hex::encode(hasher.finalize())
}

/// The earlier result of an identical redeem, if one committed.
async fn redeem_replay(
    store: &LibraryStore,
    operation_id: &str,
    request_hash: &str,
) -> Result<Option<EnrolledNode>> {
    let Some(row) = bookclerk_receipts::Entity::find_by_id(operation_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if row.operation_kind != REDEEM_KIND || row.request_hash != request_hash || row.status != "ok" {
        // Same link, different secret or keys: the link is spent.
        return Err(redeem_refused());
    }
    let receipt: RedeemReceipt = serde_json::from_str(row.payload.as_deref().unwrap_or(""))
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid redeem receipt: {err}")))?;
    Ok(Some(EnrolledNode {
        cluster_id: receipt.cluster_id,
        node_id: receipt.node_id,
        owner_user_id: receipt.owner_user_id,
        replayed: true,
    }))
}

/// A host node's stored credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeCredential {
    /// Host node.
    pub node_id: String,
    /// Ed25519 public signing key.
    pub sign_public_key: Vec<u8>,
    /// X25519 public box key.
    pub box_public_key: Vec<u8>,
    /// Link that enrolled it.
    pub link_id: String,
    /// RFC 3339 enrollment time.
    pub enrolled_at: String,
    /// RFC 3339 revoke time.
    pub revoked_at: Option<String>,
    /// RFC 3339 time of the last authenticated connection.
    pub last_connected_at: Option<String>,
}

impl From<node_credentials::Model> for NodeCredential {
    fn from(row: node_credentials::Model) -> Self {
        Self {
            node_id: row.node_id,
            sign_public_key: row.sign_public_key,
            box_public_key: row.box_public_key,
            link_id: row.link_id,
            enrolled_at: row.enrolled_at,
            revoked_at: row.revoked_at,
            last_connected_at: row.last_connected_at,
        }
    }
}

/// Reads a host node's credential.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_node_credential(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Option<NodeCredential>> {
    Ok(node_credentials::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(NodeCredential::from))
}

/// Stamps `last_connected_at` after a host node authenticates. Returns
/// false (and stamps nothing) when the credential is missing or revoked,
/// so a revoke that lands mid-handshake still refuses the connection.
///
/// # Errors
///
/// Returns an error when the update fails.
pub async fn record_node_connected(store: &LibraryStore, node_id: &str) -> Result<bool> {
    let updated = node_credentials::Entity::update_many()
        .col_expr(
            node_credentials::Column::LastConnectedAt,
            Expr::value(stamp(Utc::now())),
        )
        .filter(node_credentials::Column::NodeId.eq(node_id))
        .filter(node_credentials::Column::RevokedAt.is_null())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(updated.rows_affected == 1)
}

/// Revokes a host node's credential and tells every node, so the node
/// holding that host's connection can close it.
///
/// The operator and an Owner may revoke any host node; any other active
/// user may revoke the nodes they own. Revoking twice keeps the first time.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] when the node is unknown or not the
/// caller's (the two read the same), [`LibraryError::Invalid`] when the
/// node has no host credential (an owner node), and the write error when
/// the update fails.
pub async fn revoke_node_credential(
    store: &LibraryStore,
    actor: &EnrollmentActor,
    node_id: &str,
) -> Result<NodeCredential> {
    let resolved = resolve(store, actor).await?;
    let not_found = || LibraryError::NotFound(format!("node {node_id}"));
    let node = match super::load_node(store, node_id).await {
        Ok(node) => node,
        Err(LibraryError::NotFound(_)) => return Err(not_found()),
        Err(err) => return Err(err),
    };
    let owned = node
        .owner_user_id
        .is_some_and(|owner| resolved.covers_user(owner));
    if !matches!(resolved, Resolved::Admin { .. }) && !owned {
        return Err(not_found());
    }
    if load_node_credential(store, node_id).await?.is_none() {
        return Err(LibraryError::Invalid(format!(
            "node {node_id} has no host credential to revoke"
        )));
    }
    node_credentials::Entity::update_many()
        .col_expr(
            node_credentials::Column::RevokedAt,
            Expr::value(stamp(Utc::now())),
        )
        .filter(node_credentials::Column::NodeId.eq(node_id))
        .filter(node_credentials::Column::RevokedAt.is_null())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let credential = load_node_credential(store, node_id)
        .await?
        .ok_or_else(not_found)?;
    // A revoked branch is one that left: its shared titles are marked.
    super::branch_catalog::detach_branch_titles(store, node_id, super::DETACHED_LEFT).await?;
    tracing::info!(node_id, "revoked host node credential");
    notify_all_nodes(
        store,
        NodeMessage::NodeRevoked {
            node_id: node_id.to_string(),
        },
    )
    .await;
    Ok(credential)
}
