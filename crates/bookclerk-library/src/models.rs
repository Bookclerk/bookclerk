use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Acquire / download state for a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcquireStatus {
    /// Title has not been queued or downloaded yet.
    #[default]
    NotAcquired,
    /// Title is waiting in the acquire queue.
    Queued,
    /// Acquire pipeline is actively downloading this title.
    Downloading,
    /// Primary audio artifact is stored and library status is liberated.
    Acquired,
    /// Last acquire attempt failed; see `error_message` on the book row.
    Error,
}

impl AcquireStatus {
    /// Returns the canonical snake_case / lowercase wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotAcquired => "not_acquired",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Acquired => "acquired",
            Self::Error => "error",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_acquired" => Some(Self::NotAcquired),
            "queued" => Some(Self::Queued),
            "downloading" => Some(Self::Downloading),
            "acquired" => Some(Self::Acquired),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Classic EF `LiberatedStatus` integer.
    #[must_use]
    pub fn to_classic(self) -> i32 {
        match self {
            Self::Acquired => 1,
            Self::Error => 2,
            Self::Downloading | Self::Queued => 0x1000,
            Self::NotAcquired => 0,
        }
    }

    /// Parse classic EF `LiberatedStatus` integer.
    #[must_use]
    pub fn from_classic(status: i64) -> Self {
        match status {
            1 => Self::Acquired,
            2 => Self::Error,
            0x1000 => Self::Downloading,
            _ => Self::NotAcquired,
        }
    }
}

/// True when `content_kind` is a podcast episode.
#[must_use]
pub fn is_episode(content_kind: &str) -> bool {
    content_kind.eq_ignore_ascii_case("episode")
}

/// True when `content_kind` is a podcast parent / show (no audio to acquire).
#[must_use]
pub fn is_podcast_parent(content_kind: &str) -> bool {
    matches!(
        content_kind.to_ascii_lowercase().as_str(),
        "podcast" | "parent" | "podcastparent" | "podcast_parent" | "season"
    )
}

/// True when the title can be downloaded (classic `WithoutParents`).
#[must_use]
pub fn is_downloadable(content_kind: &str) -> bool {
    !is_podcast_parent(content_kind)
}

/// Classic EF `ContentType` enum value.
#[must_use]
pub fn content_kind_to_classic(content_kind: &str) -> i32 {
    if is_episode(content_kind) {
        2 // Episode
    } else if is_podcast_parent(content_kind) {
        4 // Parent
    } else {
        1 // Product
    }
}

/// Map classic EF `ContentType` to bookclerk `content_kind`.
#[must_use]
pub fn content_kind_from_classic(content_type: i64) -> String {
    match content_type {
        2 => String::from("episode"),
        4 => String::from("podcast"),
        _ => String::from("book"),
    }
}

/// Account row stored in the Bookclerk DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Store marketplace / locale code (for example `us`, `uk`).
    pub marketplace: String,
    /// Optional operator-facing display label.
    pub label: Option<String>,
    /// When nonzero/true, scheduled scans include this account.
    pub scan_enabled: bool,
    /// `active` or `revoked` (credentials removed; books retained).
    #[serde(default = "default_connection_status")]
    pub connection_status: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Serde default for account `connection_status` when the column is missing (`active`).
fn default_connection_status() -> String {
    String::from("active")
}

/// Portal identity bound to an external provider user (e.g. ABS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalIdentity {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// External identity or integration provider id (for example ABS).
    pub provider: String,
    /// User id at the external provider.
    pub external_user_id: String,
    /// Optional operator-facing display label.
    pub label: Option<String>,
    /// First-party user this external identity is linked to (Phase 1).
    pub user_id: Option<i64>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// Last HTTPS profile picture URL from the identity provider, when any.
    pub picture_url: Option<String>,
}

/// IdP-supplied avatar available as a profile-picture choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSsoPicture {
    /// `portal_identities.id` this picture belongs to.
    pub identity_id: i64,
    /// Identity-broker id (`oidc:google`, `oidc:github`, …).
    pub provider: String,
    /// HTTPS URL to display (hotlinked from the IdP / CDN).
    pub picture_url: String,
    /// Most recent portal session activity for this identity, when any.
    pub last_used_at: Option<DateTime<Utc>>,
}

/// First-party user role (`administrator` | `member`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    /// Host super-user: may elevate to Operator after password re-auth.
    Owner,
    /// Library / user administration without Operator elevation.
    Administrator,
    /// Library member with scoped portal access (no operator token).
    Member,
}

impl UserRole {
    /// Returns the canonical snake_case / lowercase wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Administrator => "administrator",
            Self::Member => "member",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "administrator" => Some(Self::Administrator),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    /// Whether this role may provision users and manage the host library.
    #[must_use]
    pub fn is_privileged(self) -> bool {
        matches!(self, Self::Owner | Self::Administrator)
    }

    /// Whether this role may elevate to a short-lived Operator session.
    #[must_use]
    pub fn can_elevate(self) -> bool {
        matches!(self, Self::Owner)
    }
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// First-party user status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    /// Account may sign in and use the portal.
    Active,
    /// Account is blocked from new sessions.
    Disabled,
}

impl UserStatus {
    /// Returns the canonical snake_case / lowercase wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// First-party Bookclerk user (security principal for portal paths).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// First-party role (`owner`, `administrator`, or `member`).
    pub role: UserRole,
    /// Lifecycle status for the row (user, request, …).
    pub status: UserStatus,
    /// Human-readable name shown in the UI.
    pub display_name: Option<String>,
    /// Local username for password login, when set.
    pub login_name: Option<String>,
    /// Optional contact email (notifications / magic-link invites later).
    pub email: Option<String>,
    /// Whether a local password hash is stored for this user.
    pub has_password: bool,
    /// Incremented to invalidate existing sessions after security changes.
    pub security_version: i64,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
    /// Last authenticated portal activity (RFC 3339); survives session expiry.
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Explicit picture choice (`monogram` / `gravatar` / `upload` / `sso:{id}`); `None` is auto.
    #[serde(default)]
    pub avatar_source: Option<String>,
    /// True when a TOTP authenticator has been confirmed for local password login.
    #[serde(default)]
    pub totp_enabled: bool,
}

/// Registered OIDC authorization-server client (Bookclerk as IdP).
#[derive(Debug, Clone)]
pub struct OidcClientRecord {
    /// Surrogate primary key.
    pub id: i64,
    /// Public client_id used at authorize / token.
    pub client_id: String,
    /// Hash of the client secret; `None` for public PKCE clients.
    pub client_secret_hash: Option<String>,
    /// Allowed OAuth redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Operator-facing display name.
    pub name: Option<String>,
    /// When true, token responses include a refresh token.
    pub issue_refresh_token: bool,
    /// Scopes this client may be granted (`openid`, `profile`, `email`, …).
    pub allowed_scopes: Vec<String>,
    /// When true, authorize and token endpoints accept this client.
    pub enabled: bool,
    /// Plugin id that owns this client; `None` for operator-created clients.
    pub plugin_id: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

impl OidcClientRecord {
    /// True when a client secret hash is stored (confidential client).
    #[must_use]
    pub fn has_secret(&self) -> bool {
        self.client_secret_hash
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
    }

    /// True when a plugin owns this client's identity and redirect URIs.
    #[must_use]
    pub fn is_plugin_provided(&self) -> bool {
        self.plugin_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
    }
}

/// Stored WebAuthn credential (passkey JSON plus the label shown in Settings).
#[derive(Debug, Clone)]
pub struct StoredPasskey {
    /// Surrogate primary key.
    pub id: i64,
    /// Base64url credential id.
    pub credential_id: String,
    /// Serialized `webauthn_rs` passkey JSON.
    pub passkey_json: String,
    /// Label chosen at registration; `None` for rows created before names existed.
    pub name: Option<String>,
}

/// Storefront / integration connection summary for admin user lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIntegrationHint {
    /// Content-source or integration plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Optional operator-facing account label.
    pub label: Option<String>,
}

/// Recent unfinished listening hint for admin user lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserListeningHint {
    /// Display title when the provider reported one.
    pub title: Option<String>,
    /// Integration / storefront provider id.
    pub provider: String,
    /// RFC 3339 time of the last playback update.
    pub last_listened_at: DateTime<Utc>,
}

/// Presence + connection extras for administrator user management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPresenceExtras {
    /// True when the user has a non-expired portal session.
    pub online: bool,
    /// Most recent unfinished listen within the listening window, if any.
    pub listening: Option<UserListeningHint>,
    /// Linked storefront / integration accounts across portal identities.
    pub integrations: Vec<UserIntegrationHint>,
    /// Most recent *unexpired* portal session activity (RFC 3339), when known.
    pub last_active_at: Option<DateTime<Utc>>,
}

/// Invite ticket for provisioning a User (token plaintext never stored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInviteRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// SHA-256 hex digest of the opaque token (plaintext never stored).
    pub token_hash: String,
    /// First-party role (`administrator` or `member`).
    pub role: UserRole,
    /// Local username for password login, when set.
    pub login_name: Option<String>,
    /// Human-readable name shown in the UI.
    pub display_name: Option<String>,
    /// RFC 3339 expiry for the ticket, session, or code.
    pub expires_at: DateTime<Utc>,
    /// RFC 3339 time when the ticket/invite was redeemed, if any.
    pub redeemed_at: Option<DateTime<Utc>>,
    /// Actor that created the row (user id or operator label).
    pub created_by: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
}

/// Durable operator session metadata (hashed token is never exposed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorSessionRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// RFC 3339 expiry for the ticket, session, or code.
    pub expires_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 time of the last authenticated use of this session.
    pub last_used_at: Option<DateTime<Utc>>,
    /// User id that elevated into this operator session, if any.
    pub elevated_from_user_id: Option<i64>,
    /// User id being impersonated by this operator session, if any.
    pub impersonating_user_id: Option<i64>,
    /// Raw User-Agent captured at session mint (optional).
    pub user_agent: Option<String>,
    /// Best-effort device class (`desktop` / `mobile` / `tablet` / `api`).
    pub device_type: Option<String>,
    /// Best-effort OS / client label (`Windows`, `Android`, `API`, …).
    pub client_label: Option<String>,
}

/// Portal session row for session lists (includes hash for current-session match).
#[derive(Debug, Clone)]
pub struct PortalSessionRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// SHA-256 hex of the session token (never serialized to clients).
    pub token_hash: String,
    /// RFC 3339 created timestamp.
    pub created_at: String,
    /// RFC 3339 expiry timestamp.
    pub expires_at: String,
    /// RFC 3339 last-used timestamp, when known.
    pub last_used_at: Option<String>,
    /// Raw User-Agent captured at session mint (optional).
    pub user_agent: Option<String>,
    /// Best-effort device class.
    pub device_type: Option<String>,
    /// Best-effort OS / client label.
    pub client_label: Option<String>,
}

/// Security audit event (elevate / impersonate / login / provision).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityAuditEvent {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// RFC 3339 timestamp of the audit event.
    pub at: DateTime<Utc>,
    /// Actor label (user id, operator, or system).
    pub actor: String,
    /// Audit action verb (for example `login`, `rotate_token`).
    pub action: String,
    /// JSON object with structured event details (no secrets).
    pub detail_json: Option<String>,
}

/// Claim ticket metadata (token plaintext is never stored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimTicketRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// SHA-256 hex digest of the opaque token (plaintext never stored).
    pub token_hash: String,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// RFC 3339 expiry for the ticket, session, or code.
    pub expires_at: DateTime<Utc>,
    /// RFC 3339 time when the ticket/invite was redeemed, if any.
    pub redeemed_at: Option<DateTime<Utc>>,
    /// Actor that created the row (user id or operator label).
    pub created_by: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
}

/// Link between a portal identity and a bookstore account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountLinkRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: i64,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
}

/// Book / library item (one ownership row per store product per account).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Stable UUID for this row (API / foreign-key identity).
    pub uuid: String,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Audible ASIN when known (`None` for Libro-only rows without enrichment).
    pub asin: Option<String>,
    /// ISBN-13 when known (Libro always; Audible when the API provides it).
    pub isbn: Option<String>,
    /// Store marketplace / locale code (for example `us`, `uk`).
    pub marketplace: String,
    /// Display title of the work or edition.
    pub title: String,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    pub series_index: Option<String>,
    /// Audible series / podcast-parent ASIN (`Series.AudibleSeriesId`).
    pub series_asin: Option<String>,
    /// Download/acquire pipeline state (`not_acquired`, `queued`, …).
    pub acquire_status: AcquireStatus,
    /// Object-storage key for the primary audio artifact, if acquired.
    pub storage_key: Option<String>,
    /// Last acquire/convert failure message for operators.
    pub error_message: Option<String>,
    /// RFC 3339 purchase time from the storefront, when known.
    pub purchased_at: Option<DateTime<Utc>>,
    /// Operator or storefront tags (serialized string).
    pub tags: Option<String>,
    /// Overall user rating from the storefront, if any.
    pub rating_overall: Option<f32>,
    /// Narration/performance rating from the storefront, if any.
    pub rating_performance: Option<f32>,
    /// Story rating from the storefront, if any.
    pub rating_story: Option<f32>,
    /// Whether the listener marked the title finished (0/1 or bool).
    pub is_finished: bool,
    /// Companion PDF acquire state (`not_acquired`, `acquired`, …).
    pub pdf_status: AcquireStatus,
    /// Object-storage key for the companion PDF, if present.
    pub pdf_storage_key: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    pub length_minutes: Option<i64>,
    /// Whether the edition is abridged (0/1 or bool).
    pub is_abridged: bool,
    /// Title kind: `book`, `episode`, `podcast`, ….
    pub content_kind: String,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    pub subtitle: Option<String>,
    /// Publication date string from the storefront or enrichment.
    pub published_at: Option<DateTime<Utc>>,
    /// Blurb / synopsis text (may contain HTML).
    pub description: Option<String>,
    /// BCP-47 or storefront language code when known.
    pub language: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Subject / topic tags (often from Open Library; `;`- or `,`-separated).
    pub subjects: Option<String>,
    /// Plugin or catalog that last enriched bibliographic fields.
    pub enrich_source: Option<String>,
    /// 0–1 confidence score for the last enrichment pass.
    pub enrich_confidence: Option<f64>,
    /// RFC 3339 time of the last enrichment write.
    pub enrich_updated_at: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Canonical work spanning one or more ownership rows / editions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRecord {
    /// Surrogate primary key assigned by the database.
    pub id: String,
    /// Preferred ASIN representing this canonical work.
    pub canonical_asin: Option<String>,
    /// Preferred ISBN representing this canonical work.
    pub canonical_isbn: Option<String>,
    /// Display title of the work or edition.
    pub title: String,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    pub narrators: Option<String>,
    /// Blurb / synopsis text (may contain HTML).
    pub description: Option<String>,
    /// Subject / topic tags from enrichment (serialized).
    pub subjects: Option<String>,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// BCP-47 or storefront language code when known.
    pub language: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    pub series_index: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Open Library work/edition id when enrichment found one.
    pub openlibrary_id: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Listening progress snapshot from an external player (e.g. AudioBookshelf).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListeningProgressRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// External identity or integration provider id (for example ABS).
    pub provider: String,
    /// User id at the external provider.
    pub external_user_id: String,
    /// Foreign key to `books.uuid`.
    pub book_uuid: Option<String>,
    /// Canonical work id this edition or request resolves to.
    pub work_id: Option<String>,
    /// Provider-native listening-progress item id.
    pub external_item_id: String,
    /// Display title of the work or edition.
    pub title: Option<String>,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Fractional progress 0.0–1.0 when the provider reports it.
    pub progress: Option<f64>,
    /// Current playback position within the title, in seconds.
    pub current_time_seconds: Option<f64>,
    /// Total duration in seconds when known.
    pub duration_seconds: Option<f64>,
    /// Whether the listener marked the title finished (0/1 or bool).
    pub is_finished: bool,
    /// RFC 3339 time of the last playback update.
    pub last_listened_at: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Wishlist row status (`open` while wishlisted; `cancelled` after un-wishlist).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    /// Wishlist item is still open (not fulfilled/cancelled).
    #[default]
    Open,
    /// Request cancelled by the requester or operator.
    Cancelled,
}

impl RequestStatus {
    /// Returns the canonical snake_case / lowercase wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            // Legacy triage statuses collapse to cancelled (no approval flow).
            "cancelled" | "approved" | "acquired" | "rejected" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Per-storefront catalog/pricing snapshot attached to a wishlist row.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TitleRequestSourceRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Foreign key to `title_requests.id`.
    pub title_request_id: i64,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Display title of the work or edition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Comma-separated or JSON author list from the storefront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_index: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Blurb / synopsis text (may contain HTML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_minutes: Option<i64>,
    /// Publication date string from the storefront or enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Storefront category / genre path list (serialized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<String>,
    /// BCP-47 or storefront language code when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// HTTPS URL for cover art when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// Storefront product or purchase URL when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Observed price in minor currency units, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Storefront-formatted price string for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_label: Option<String>,
    /// List/MSRP price in minor units, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_cents: Option<i64>,
    /// Storefront-formatted list price for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_label: Option<String>,
    /// Member/subscriber price in minor units, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_cents: Option<i64>,
    /// Storefront-formatted member price for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_label: Option<String>,
    /// RFC 3339 time when this storefront snapshot was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Storefront edition key for wishlist / queue payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WishlistStoreEdition {
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
}

/// Snapshotted purchase link/price for a wishlist title.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WishlistPurchaseHint {
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Display title of the work or edition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Storefront product or purchase URL when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Observed price in minor currency units, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Storefront-formatted price string for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_label: Option<String>,
    /// List/MSRP price in minor units, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_cents: Option<i64>,
    /// Storefront-formatted list price for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_label: Option<String>,
    /// Member/subscriber price in minor units, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_cents: Option<i64>,
    /// Storefront-formatted member price for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_label: Option<String>,
}

/// Personal wishlist row (also contributes to the shared global queue while open).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleRequestRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Stable UUID for this row (API / foreign-key identity).
    pub uuid: String,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// Display title of the work or edition.
    pub title: String,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Free-form operator or requester notes.
    pub notes: Option<String>,
    /// Lifecycle status for the row (user, request, …).
    pub status: RequestStatus,
    /// Stable bibliographic key (`isbn:…` / `asin:…` / `soft:…`) for aggregation.
    #[serde(default)]
    pub work_key: String,
    /// Canonical work id this edition or request resolves to.
    pub work_id: Option<String>,
    /// Library `books.uuid` once the wishlist item is fulfilled.
    pub resolved_book_uuid: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Per-storefront snapshots (empty for legacy rows).
    #[serde(default)]
    pub sources: Vec<TitleRequestSourceRecord>,
    /// Blurb / synopsis text (may contain HTML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_index: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_minutes: Option<i64>,
    /// Publication date string from the storefront or enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Merged genre / category list for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genres: Option<String>,
    /// BCP-47 or storefront language code when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Distinct storefront editions contributing to this wishlist item.
    #[serde(default)]
    pub store_editions: Vec<WishlistStoreEdition>,
    /// Purchase URLs and prices observed per storefront.
    #[serde(default)]
    pub purchase_hints: Vec<WishlistPurchaseHint>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Compact wisher shown on the global queue (avatar + label).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueWisher {
    /// First-party `users.id` when the portal identity is linked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    /// Portal identity that created the wish (dedupe for unlinked identities).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<i64>,
    /// Display name, identity label, or `"Operator"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Local login name when the first-party user has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_name: Option<String>,
    /// True when the wish came from the operator token (no portal user).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub operator: bool,
    /// True when an uploaded avatar file exists (filled by the daemon).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_avatar: bool,
    /// Explicit picture choice when the user has set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_source: Option<String>,
    /// SHA-256 hex of the contact email for Gravatar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gravatar_hash: Option<String>,
    /// HTTPS picture from the wishing identity, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_url: Option<String>,
}

impl QueueWisher {
    /// Returns true when both rows represent the same person for aggregation.
    ///
    /// # Arguments
    ///
    /// * `other` - Candidate wisher to compare.
    ///
    /// # Returns
    ///
    /// True for two operator wishes, the same `user_id`, or the same unlinked identity.
    #[must_use]
    pub fn same_person(&self, other: &Self) -> bool {
        if self.operator && other.operator {
            return true;
        }
        if let (Some(a), Some(b)) = (self.user_id, other.user_id) {
            return a == b;
        }
        match (self.identity_id, other.identity_id) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

/// Aggregated global request-queue entry (one work, many wishers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalQueueEntry {
    /// Stable merge key used to group editions into a work.
    pub work_key: String,
    /// Display title of the work or edition.
    pub title: String,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Blurb / synopsis text (may contain HTML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_index: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_minutes: Option<i64>,
    /// Publication date string from the storefront or enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Merged genre / category list for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genres: Option<String>,
    /// BCP-47 or storefront language code when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Distinct storefront editions contributing to this wishlist item.
    #[serde(default)]
    pub store_editions: Vec<WishlistStoreEdition>,
    /// Purchase URLs and prices observed per storefront.
    #[serde(default)]
    pub purchase_hints: Vec<WishlistPurchaseHint>,
    /// Number of users who requested this title.
    pub wish_count: i64,
    /// Distinct people who have this title on an open wishlist.
    #[serde(default)]
    pub wishers: Vec<QueueWisher>,
    /// Sample title-request UUIDs represented in this aggregate row.
    pub sample_uuids: Vec<String>,
    /// Earliest request timestamp in this aggregate group.
    pub first_requested_at: DateTime<Utc>,
    /// Most recent request timestamp in this aggregate group.
    pub last_requested_at: DateTime<Utc>,
}

/// Maximum distinct people serialized on a global-queue or Discover card.
pub const MAX_QUEUE_WISHERS: usize = 16;

/// Adds `wisher` when they are not already present (capped at [`MAX_QUEUE_WISHERS`]).
///
/// Duplicate people keep the richer picture and name fields.
///
/// # Arguments
///
/// * `wishers` - Accumulated people for this work.
/// * `wisher` - Person who has this title on an open wishlist.
pub fn push_queue_wisher(wishers: &mut Vec<QueueWisher>, wisher: QueueWisher) {
    if let Some(existing) = wishers.iter_mut().find(|w| w.same_person(&wisher)) {
        if existing
            .picture_url
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            existing.picture_url = wisher.picture_url;
        }
        if existing.gravatar_hash.is_none() {
            existing.gravatar_hash = wisher.gravatar_hash;
        }
        if existing
            .display_name
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            existing.display_name = wisher.display_name;
        }
        if existing.login_name.is_none() {
            existing.login_name = wisher.login_name;
        }
        if existing.avatar_source.is_none() {
            existing.avatar_source = wisher.avatar_source;
        }
        existing.has_avatar |= wisher.has_avatar;
        return;
    }
    if wishers.len() < MAX_QUEUE_WISHERS {
        wishers.push(wisher);
    }
}

impl GlobalQueueEntry {
    /// Adds `wisher` when they are not already present (capped at 16).
    ///
    /// Duplicate people keep the richer picture and name fields.
    ///
    /// # Arguments
    ///
    /// * `wisher` - Person who has this title on an open wishlist.
    pub fn push_wisher(&mut self, wisher: QueueWisher) {
        push_queue_wisher(&mut self.wishers, wisher);
    }
}

/// Stored embedding vector metadata (blob fetched separately when needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Embedding target kind (`book`, `work`, …).
    pub target_kind: String,
    /// Id of the embedded target within `target_kind`.
    pub target_id: String,
    /// Embedding model identifier used to produce `vector`.
    pub model: String,
    /// Dimensionality of the stored embedding vector.
    pub dims: i64,
    /// Hash of the text that was embedded (skip re-embed when unchanged).
    pub text_hash: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

/// Per-user GUI / Discover preferences (operator or portal identity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Preference subject key (`operator`, `user:<id>`, portal key).
    pub subject_key: String,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// Preferred library SPA view identifier (for example `grid` or `list`).
    pub default_view: String,
    /// Shelf kind ids to hide (`author`, `chirp_deals`, …). Empty = all on.
    pub disabled_shelves: Vec<String>,
    /// Preferred Discover sort column (for example `title` or `added`).
    pub discover_sort: String,
    /// Discover sort direction (`asc` / `desc`).
    pub discover_sort_dir: String,
    /// Preferred content language (`en`, `__all__`, …). `None` = browser default.
    pub discover_language: Option<String>,
    /// Store ids to hide in Discover. Empty = all sources (including future).
    pub discover_excluded_sources: Vec<String>,
    /// Appearance preference (`system`, `light`, or `dark`).
    pub theme: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: DateTime<Utc>,
}

impl UserPreferences {
    /// Defaults when no row exists yet.
    #[must_use]
    pub fn defaults_for(subject_key: &str, identity_id: Option<i64>) -> Self {
        Self {
            id: 0,
            subject_key: subject_key.to_string(),
            identity_id,
            default_view: String::from("discover"),
            disabled_shelves: Vec::new(),
            discover_sort: String::from("relevance"),
            discover_sort_dir: String::from("desc"),
            discover_language: None,
            discover_excluded_sources: Vec::new(),
            theme: String::from("system"),
            updated_at: Utc::now(),
        }
    }
}

/// Subject key for the shared operator account.
pub const OPERATOR_PREFS_KEY: &str = "operator";

/// Subject key for a portal identity (legacy; prefer [`user_prefs_key`]).
#[must_use]
pub fn portal_prefs_key(identity_id: i64) -> String {
    format!("portal:{identity_id}")
}

/// Subject key for a first-party user.
#[must_use]
pub fn user_prefs_key(user_id: i64) -> String {
    format!("user:{user_id}")
}

/// Canonical appearance preference; unknown values become `system`.
#[must_use]
pub fn normalize_theme(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "light" => String::from("light"),
        "dark" => String::from("dark"),
        _ => String::from("system"),
    }
}

/// Kind of durable daemon work stored in the `jobs` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Library scan for one account or every scan-enabled account.
    Scan,
    /// Acquire pending titles (optional title / account filter).
    Acquire,
    /// Sync listening progress from capable integrations.
    ListenSync,
    /// Remote integration library scan (Audiobookshelf, …).
    IntegrationScan,
    /// ABI v2 `JobHandler` stream-copy vertical slice (`plugin_copy`).
    PluginCopy,
    /// Terminal placeholder after an unreadable persisted command is rejected.
    Invalid,
}

impl JobKind {
    /// Returns the canonical snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Acquire => "acquire",
            Self::ListenSync => "listen_sync",
            Self::IntegrationScan => "integration_scan",
            Self::PluginCopy => "plugin_copy",
            Self::Invalid => "invalid",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "scan" => Some(Self::Scan),
            "acquire" => Some(Self::Acquire),
            "listen_sync" => Some(Self::ListenSync),
            "integration_scan" => Some(Self::IntegrationScan),
            "plugin_copy" => Some(Self::PluginCopy),
            "invalid" => Some(Self::Invalid),
            _ => None,
        }
    }

    /// Resource class used for admission and worker concurrency.
    #[must_use]
    pub fn resource_class(self) -> JobResourceClass {
        JobResourceClass::Network
    }

    /// Idempotency key unique among pending/running jobs of the same work.
    #[must_use]
    pub fn dedup_key(self, payload: &JobPayload) -> String {
        let account = payload.account.as_deref().unwrap_or("all");
        match self {
            Self::Scan => format!("scan:account={account}"),
            Self::Acquire => {
                let title = payload.title.as_deref().unwrap_or("all");
                format!("acquire:title={title}:account={account}")
            }
            Self::ListenSync => "listen_sync".into(),
            Self::IntegrationScan => {
                let id = payload.integration_id.as_deref().unwrap_or("all");
                format!("integration_scan:id={id}:force={}", u8::from(payload.force))
            }
            Self::PluginCopy => {
                let plugin = payload.plugin_id.as_deref().unwrap_or("local");
                let from = payload.source_key.as_deref().unwrap_or("-");
                let to = payload.dest_key.as_deref().unwrap_or("-");
                format!("plugin_copy:plugin={plugin}:from={from}:to={to}")
            }
            Self::Invalid => "invalid".into(),
        }
    }
}

/// Lifecycle state of a durable job row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Admitted and waiting to be claimed.
    Pending,
    /// Claimed by a worker that holds a live lease.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Exhausted retries or failed without retry.
    Failed,
    /// Cancelled by an operator before or during execution.
    Cancelled,
}

impl JobState {
    /// Returns the canonical snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// True when the job may still be claimed or is executing.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    /// True when the job will not run again.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Concurrency class that bounds how many jobs of this type run at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobResourceClass {
    /// Store HTTP / scan / acquire / listening-sync work.
    Network,
    /// Codec / remux work (reserved; media pool remains separate).
    Media,
    /// Speech-to-text / derived transcript work (reserved for #121).
    Transcription,
    /// Search-index / embedding work (reserved).
    Indexing,
}

impl JobResourceClass {
    /// Returns the canonical snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Media => "media",
            Self::Transcription => "transcription",
            Self::Indexing => "indexing",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "network" => Some(Self::Network),
            "media" => Some(Self::Media),
            "transcription" => Some(Self::Transcription),
            "indexing" => Some(Self::Indexing),
            _ => None,
        }
    }

    /// Canonical wire strings for every known class.
    pub const ALL: &'static [&'static str] = &["network", "media", "transcription", "indexing"];
}

/// Who admitted the job (API vs periodic scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobTrigger {
    /// Enqueued from the HTTP control plane, tray, or CLI daemon commands.
    #[default]
    Api,
    /// Enqueued by the daemon interval scheduler.
    Scheduler,
}

impl JobTrigger {
    /// Returns the canonical snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Scheduler => "scheduler",
        }
    }

    /// Parses the canonical wire string; returns `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "api" => Some(Self::Api),
            "scheduler" => Some(Self::Scheduler),
            _ => None,
        }
    }
}

/// Current job command envelope version stored in [`JobPayload::v`].
pub const JOB_PAYLOAD_VERSION: u32 = 1;

/// JSON command envelope stored on a job row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobPayload {
    /// Envelope schema version; must equal [`JOB_PAYLOAD_VERSION`].
    #[serde(default = "default_job_payload_version")]
    pub v: u32,
    /// Optional account filter (`None` = all eligible accounts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Optional title filter (uuid / ASIN / ISBN / product id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Admission source (`api` or `scheduler`).
    #[serde(default)]
    pub trigger: JobTrigger,
    /// Integration plugin id for [`JobKind::IntegrationScan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    /// When true, an integration scan asks the remote to rescan.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// Destination plugin id for [`JobKind::PluginCopy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Source object key for [`JobKind::PluginCopy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// Destination object key for [`JobKind::PluginCopy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_key: Option<String>,
    /// Bounded checkpoint restored after [`bookclerk_plugin_abi::v2::JobOutcome::Suspended`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<bookclerk_plugin_abi::v2::JobCheckpoint>,
    /// Resume ordinal; distinct from failure [`Self`] attempt_count on the job row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_sequence: Option<u32>,
    /// One-shot: the next claim is a suspend-resume and must not consume a failure attempt.
    ///
    /// Cleared when that claim is taken. A leftover [`Self::checkpoint`] after a
    /// retryable failure must not keep later claims in the resume path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume_pending: bool,
}

/// Default envelope version for missing `v` on well-formed legacy rows.
fn default_job_payload_version() -> u32 {
    JOB_PAYLOAD_VERSION
}

impl Default for JobPayload {
    fn default() -> Self {
        Self {
            v: JOB_PAYLOAD_VERSION,
            account: None,
            title: None,
            trigger: JobTrigger::Api,
            integration_id: None,
            force: false,
            plugin_id: None,
            source_key: None,
            dest_key: None,
            checkpoint: None,
            invocation_sequence: None,
            resume_pending: false,
        }
    }
}

/// Admission request for [`crate::LibraryStore::enqueue_job`].
#[derive(Debug, Clone)]
pub struct EnqueueJobSpec {
    /// Job kind to run.
    pub kind: JobKind,
    /// Kind-specific filters and trigger metadata.
    pub payload: JobPayload,
    /// Higher values are claimed first (default 0).
    pub priority: i64,
    /// Maximum claims before a failure is terminal.
    pub max_attempts: i64,
    /// Global cap on pending+running rows; enqueue fails when at the cap.
    pub max_pending: i64,
    /// Optional delay before the job becomes claimable.
    pub run_after: Option<DateTime<Utc>>,
}

/// Result of admitting a job into the durable queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A new row was inserted.
    Created {
        /// New job id.
        id: String,
    },
    /// An equivalent pending/running job already exists.
    Duplicate {
        /// Existing job id that satisfies the dedup key.
        existing_id: String,
    },
    /// `pending + running` already equals `max_pending`.
    QueueFull,
}

/// Durable job row returned to the daemon API and worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Stable job id (`scan-{uuid}`, …).
    pub id: String,
    /// Job kind (`scan`, `acquire`, `listen_sync`).
    pub kind: JobKind,
    /// Lifecycle state.
    pub state: JobState,
    /// Higher values are claimed first.
    pub priority: i64,
    /// Concurrency class.
    pub resource_class: JobResourceClass,
    /// Kind-specific filters and trigger metadata.
    pub payload: JobPayload,
    /// Human-readable progress string.
    pub progress: Option<String>,
    /// Number of times a worker has claimed this row.
    pub attempt_count: i64,
    /// Maximum claims before a failure becomes terminal.
    pub max_attempts: i64,
    /// Earliest time a pending job may be claimed.
    pub run_after: DateTime<Utc>,
    /// Worker id that currently holds the lease.
    pub lease_owner: Option<String>,
    /// Lease expiry; stale running rows are reclaimed after this.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Idempotency key among pending/running rows.
    pub dedup_key: String,
    /// Structured error kind when failed or cancelled.
    pub error_kind: Option<String>,
    /// Operator-facing error text.
    pub error_message: Option<String>,
    /// Cooperative cancel flag.
    pub cancel_requested: bool,
    /// When the row was inserted.
    pub created_at: DateTime<Utc>,
    /// When the row was last modified.
    pub updated_at: DateTime<Utc>,
    /// When a worker first claimed the row.
    pub started_at: Option<DateTime<Utc>>,
    /// When the row reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
    /// Per-claim generation used to fence heartbeat and finalization.
    pub lease_generation: i64,
}

impl JobRecord {
    /// Lease identity for heartbeat and finalization, when this row is claimed.
    #[must_use]
    pub fn fence(&self) -> Option<JobFence> {
        Some(JobFence {
            job_id: self.id.clone(),
            owner: self.lease_owner.clone()?,
            generation: self.lease_generation,
        })
    }
}

/// Lease identity a worker must present to mutate a running job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFence {
    /// Job id that was claimed.
    pub job_id: String,
    /// Worker id stored in `lease_owner`.
    pub owner: String,
    /// `lease_generation` assigned by the successful claim.
    pub generation: i64,
}

/// Scratch path registered against a job for crash cleanup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTempPath {
    /// Surrogate primary key.
    pub id: i64,
    /// Owning job id.
    pub job_id: String,
    /// Absolute filesystem path.
    pub path: String,
    /// When the path was registered.
    pub created_at: DateTime<Utc>,
    /// Bytes reserved against the job temp quota for this path.
    pub reserved_bytes: u64,
}

/// Next `run_after` after a failed attempt, exponential from a 5s base (capped).
#[must_use]
pub fn job_backoff_run_after(attempt_count: i64, now: DateTime<Utc>) -> DateTime<Utc> {
    let exp = u32::try_from(attempt_count.saturating_sub(1).clamp(0, 8)).unwrap_or(0);
    let secs = 5u64.saturating_mul(2u64.saturating_pow(exp)).min(15 * 60);
    now + chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX))
}

impl BookRecord {
    /// Public stable id used for CLI / API / acquire lookups.
    #[must_use]
    pub fn title_id(&self) -> &str {
        &self.uuid
    }

    /// Naming / display fallback: ASIN, else ISBN, else source product id.
    #[must_use]
    pub fn asin_or_isbn(&self) -> &str {
        self.asin
            .as_deref()
            .or(self.isbn.as_deref())
            .unwrap_or(self.product_id.as_str())
    }

    /// Source-native id for download / fetch APIs (`product_id`).
    ///
    /// Always the owning store's id (Audible ASIN or Libro ISBN), never an
    /// enrichment ASIN copied onto a non-Audible row.
    #[must_use]
    pub fn download_product_id(&self) -> &str {
        self.product_id.as_str()
    }

    /// Audible ASIN for Audible license / catalog APIs when one is known.
    #[must_use]
    pub fn audible_asin(&self) -> Option<&str> {
        self.asin.as_deref()
    }
}
