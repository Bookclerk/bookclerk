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
    /// Most recent portal session activity (RFC 3339), when known.
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
    /// Sample title-request UUIDs represented in this aggregate row.
    pub sample_uuids: Vec<String>,
    /// Earliest request timestamp in this aggregate group.
    pub first_requested_at: DateTime<Utc>,
    /// Most recent request timestamp in this aggregate group.
    pub last_requested_at: DateTime<Utc>,
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
