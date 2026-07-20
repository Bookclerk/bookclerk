use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Liberate / download state for a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LiberateStatus {
    #[default]
    NotLiberated,
    Queued,
    Downloading,
    Liberated,
    Error,
}

impl LiberateStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotLiberated => "not_liberated",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Liberated => "liberated",
            Self::Error => "error",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_liberated" => Some(Self::NotLiberated),
            "queued" => Some(Self::Queued),
            "downloading" => Some(Self::Downloading),
            "liberated" => Some(Self::Liberated),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Classic EF `LiberatedStatus` integer.
    #[must_use]
    pub fn to_classic(self) -> i32 {
        match self {
            Self::Liberated => 1,
            Self::Error => 2,
            Self::Downloading | Self::Queued => 0x1000,
            Self::NotLiberated => 0,
        }
    }

    /// Parse classic EF `LiberatedStatus` integer.
    #[must_use]
    pub fn from_classic(status: i64) -> Self {
        match status {
            1 => Self::Liberated,
            2 => Self::Error,
            0x1000 => Self::Downloading,
            _ => Self::NotLiberated,
        }
    }
}

/// True when `content_kind` is a podcast episode.
#[must_use]
pub fn is_episode(content_kind: &str) -> bool {
    content_kind.eq_ignore_ascii_case("episode")
}

/// True when `content_kind` is a podcast parent / show (no audio to liberate).
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

/// Map classic EF `ContentType` to libation-rs `content_kind`.
#[must_use]
pub fn content_kind_from_classic(content_type: i64) -> String {
    match content_type {
        2 => String::from("episode"),
        4 => String::from("podcast"),
        _ => String::from("book"),
    }
}

/// Account row stored in the Libation DB (mirrors audible-rs accounts lightly).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: i64,
    pub account_id: String,
    pub marketplace: String,
    pub label: Option<String>,
    pub scan_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Book / library item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookRecord {
    pub id: i64,
    pub asin: String,
    pub account_id: String,
    pub marketplace: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    /// Audible series / podcast-parent ASIN (`Series.AudibleSeriesId`).
    pub series_asin: Option<String>,
    pub liberate_status: LiberateStatus,
    /// Storage key (not necessarily a local path) after liberate.
    pub storage_key: Option<String>,
    pub error_message: Option<String>,
    pub purchased_at: Option<DateTime<Utc>>,
    /// Space-separated user tags (classic `UserDefinedItem.Tags`).
    pub tags: Option<String>,
    pub rating_overall: Option<f32>,
    pub rating_performance: Option<f32>,
    pub rating_story: Option<f32>,
    pub is_finished: bool,
    pub pdf_status: LiberateStatus,
    pub pdf_storage_key: Option<String>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub is_abridged: bool,
    /// `book`, `episode`, `podcast`, etc. (classic scan metadata).
    pub content_kind: String,
    pub categories: Option<String>,
    pub subtitle: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
