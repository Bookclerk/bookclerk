use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Acquire / download state for a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcquireStatus {
    #[default]
    NotAcquired,
    Queued,
    Downloading,
    Acquired,
    Error,
}

impl AcquireStatus {
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
    pub id: i64,
    pub account_id: String,
    /// `audible` or `libro`.
    pub source: String,
    pub marketplace: String,
    pub label: Option<String>,
    pub scan_enabled: bool,
    /// `active` or `revoked` (credentials removed; books retained).
    #[serde(default = "default_connection_status")]
    pub connection_status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_connection_status() -> String {
    String::from("active")
}

/// Portal identity bound to an external provider user (e.g. ABS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalIdentity {
    pub id: i64,
    pub provider: String,
    pub external_user_id: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Claim ticket metadata (token plaintext is never stored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimTicketRecord {
    pub id: i64,
    pub token_hash: String,
    pub identity_id: Option<i64>,
    pub expires_at: DateTime<Utc>,
    pub redeemed_at: Option<DateTime<Utc>>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// Link between a portal identity and a bookstore account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountLinkRecord {
    pub id: i64,
    pub identity_id: i64,
    pub account_id: String,
    pub source: String,
    pub created_at: DateTime<Utc>,
}

/// Book / library item (one ownership row per store product per account).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookRecord {
    pub id: i64,
    /// Public stable id (CLI / API / acquire target).
    pub uuid: String,
    /// `audible` or `libro`.
    pub source: String,
    pub account_id: String,
    /// Source-native product key (Audible ASIN or Libro ISBN).
    pub product_id: String,
    /// Audible ASIN when known (`None` for Libro-only rows without enrichment).
    pub asin: Option<String>,
    /// ISBN-13 when known (Libro always; Audible when the API provides it).
    pub isbn: Option<String>,
    pub marketplace: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    /// Audible series / podcast-parent ASIN (`Series.AudibleSeriesId`).
    pub series_asin: Option<String>,
    pub acquire_status: AcquireStatus,
    /// Storage key (not necessarily a local path) after acquire.
    pub storage_key: Option<String>,
    pub error_message: Option<String>,
    pub purchased_at: Option<DateTime<Utc>>,
    /// Space-separated user tags (classic `UserDefinedItem.Tags`).
    pub tags: Option<String>,
    pub rating_overall: Option<f32>,
    pub rating_performance: Option<f32>,
    pub rating_story: Option<f32>,
    pub is_finished: bool,
    pub pdf_status: AcquireStatus,
    pub pdf_storage_key: Option<String>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub is_abridged: bool,
    /// `book`, `episode`, `podcast`, etc. (classic scan metadata).
    pub content_kind: String,
    pub categories: Option<String>,
    pub subtitle: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    /// Blurb / description from enrichment or store APIs.
    pub description: Option<String>,
    pub language: Option<String>,
    pub cover_url: Option<String>,
    /// Subject / topic tags (often from Open Library; `;`- or `,`-separated).
    pub subjects: Option<String>,
    /// Last enrichment provider (`audible`, `openlibrary`, …).
    pub enrich_source: Option<String>,
    pub enrich_confidence: Option<f64>,
    pub enrich_updated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Canonical work spanning one or more ownership rows / editions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRecord {
    pub id: String,
    pub canonical_asin: Option<String>,
    pub canonical_isbn: Option<String>,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub description: Option<String>,
    pub subjects: Option<String>,
    pub categories: Option<String>,
    pub language: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub cover_url: Option<String>,
    pub openlibrary_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Listening progress snapshot from an external player (e.g. AudioBookshelf).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListeningProgressRecord {
    pub id: i64,
    pub identity_id: Option<i64>,
    pub provider: String,
    pub external_user_id: String,
    pub book_uuid: Option<String>,
    pub work_id: Option<String>,
    pub external_item_id: String,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub progress: Option<f64>,
    pub current_time_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub is_finished: bool,
    pub last_listened_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// Wishlist row status (`open` while wishlisted; `cancelled` after un-wishlist).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    #[default]
    Open,
    Cancelled,
}

impl RequestStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Cancelled => "cancelled",
        }
    }

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

/// Personal wishlist row (also contributes to the shared global queue while open).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleRequestRecord {
    pub id: i64,
    pub uuid: String,
    /// `None` = operator-submitted.
    pub identity_id: Option<i64>,
    pub title: String,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub notes: Option<String>,
    pub status: RequestStatus,
    /// Stable bibliographic key (`isbn:…` / `asin:…` / `soft:…`) for aggregation.
    #[serde(default)]
    pub work_key: String,
    pub work_id: Option<String>,
    pub resolved_book_uuid: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Aggregated global request-queue entry (one work, many wishers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalQueueEntry {
    pub work_key: String,
    pub title: String,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub wish_count: i64,
    pub sample_uuids: Vec<String>,
    pub first_requested_at: DateTime<Utc>,
    pub last_requested_at: DateTime<Utc>,
}

/// Stored embedding vector metadata (blob fetched separately when needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    pub id: i64,
    pub target_kind: String,
    pub target_id: String,
    pub model: String,
    pub dims: i64,
    pub text_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Per-user GUI / Discover preferences (operator or portal identity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    pub id: i64,
    /// `operator` or `portal:{identity_id}`.
    pub subject_key: String,
    pub identity_id: Option<i64>,
    /// `discover` | `library` | `accounts`.
    pub default_view: String,
    /// Shelf kind ids to hide (`author`, `chirp_deals`, …). Empty = all on.
    pub disabled_shelves: Vec<String>,
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
            updated_at: Utc::now(),
        }
    }
}

/// Subject key for the shared operator account.
pub const OPERATOR_PREFS_KEY: &str = "operator";

/// Subject key for a portal identity.
#[must_use]
pub fn portal_prefs_key(identity_id: i64) -> String {
    format!("portal:{identity_id}")
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
