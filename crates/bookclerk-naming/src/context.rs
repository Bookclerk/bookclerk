//! Book / chapter / contributor context passed into template evaluation.
//!
//! # Audience
//!
//! Host code building a [`BookContext`] from library rows before calling
//! [`crate::expand_template`] / [`crate::expand_filename`].

use chrono::NaiveDateTime;

/// An author or narrator (parsed lazily into name components).
#[derive(Debug, Clone, Default)]
pub struct Contributor {
    /// Display name as shown in templates (`<author>`, `<narrator>`, …).
    pub name: String,
    /// Optional store-native person id when known.
    pub id: Option<String>,
}

impl Contributor {
    /// Build a contributor from a display name and optional store id.
    pub fn new(name: impl Into<String>, id: Option<String>) -> Self {
        Self {
            name: name.into(),
            id,
        }
    }
}

/// A series membership.
#[derive(Debug, Clone, Default)]
pub struct Series {
    /// Series display name (`<series>`, `<first series>`, …).
    pub name: String,
    /// Raw order string (e.g. `"1"`, `"1-5"`, `"6"`).
    pub order: Option<String>,
    /// Optional store-native series id when known.
    pub id: Option<String>,
}

impl Series {
    /// Build a series membership from name, optional order, and optional id.
    pub fn new(name: impl Into<String>, order: Option<String>, id: Option<String>) -> Self {
        Self {
            name: name.into(),
            order,
            id,
        }
    }
}

/// The kind of content a book represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentKind {
    /// Ordinary audiobook title (default).
    #[default]
    Book,
    /// Podcast episode acquired as a child of a show.
    Episode,
    /// Podcast show / feed treated as a liberatable title.
    Podcast,
    /// Parent podcast container (show) without a single episode payload.
    PodcastParent,
}

/// All data required to evaluate a naming template for a book.
#[derive(Debug, Clone, Default)]
pub struct BookContext {
    /// Canonical title id (ISBN / ASIN fallback) — used by `<id>`.
    pub isbn: String,
    /// Audible title (no subtitle) — used by `<audible title>` and `<title short>`.
    pub title: Option<String>,
    /// Audible subtitle — used by `<audible subtitle>`.
    pub subtitle: Option<String>,
    /// Full title with subtitle — used by `<title>`. Falls back to [`Self::title`].
    pub title_with_subtitle: Option<String>,

    /// Authors in display order for `<author>` / `<authors>` / `<first author>`.
    pub authors: Vec<Contributor>,
    /// Narrators in display order for `<narrator>` / `<narrators>`.
    pub narrators: Vec<Contributor>,
    /// Series memberships for `<series>` / `<series#>` / conditionals.
    pub series: Vec<Series>,
    /// Free-form tags for `<tags>` / `[tag]` search parity helpers.
    pub tags: Vec<String>,

    /// Store account id string for `<account>` when set.
    pub account: Option<String>,
    /// Operator nickname for the account (`<account nickname>`).
    pub account_nickname: Option<String>,
    /// Marketplace / locale code (`us`, `uk`, …) for `<locale>`.
    pub locale: Option<String>,
    /// Content language for `<language>`.
    pub language: Option<String>,

    /// Publication year for `<year>` (Gregorian calendar).
    pub year_published: Option<i32>,
    /// Publication date — used by `<pub date>`.
    pub published_at: Option<NaiveDateTime>,
    /// Date added to the account — used by `<date added>`.
    pub purchased_at: Option<NaiveDateTime>,
    /// File date — used by `<file date>`.
    pub file_date: Option<NaiveDateTime>,

    /// Runtime in minutes for `<duration>` / related tags (may be fractional).
    pub length_minutes: Option<f64>,
    /// Audio bitrate in kbps when known.
    pub bitrate: Option<i64>,
    /// Sample rate in Hz when known.
    pub samplerate: Option<i64>,
    /// Channel count when known (`1` = mono, `2` = stereo).
    pub channels: Option<i64>,
    /// Codec / container label when known (`aac`, `mp3`, …).
    pub codec: Option<String>,

    /// Whether the title is abridged (`<abridged>` conditional).
    pub is_abridged: bool,
    /// Book vs podcast/episode classification for podcast-aware templates.
    pub content_kind: ContentKind,

    /// Publisher imprint for `<publisher>`.
    pub publisher: Option<String>,
    /// Genre / category labels for `<genre>` / `<categories>`.
    pub categories: Vec<String>,

    /// Bookclerk package version string for `<bookclerk version>` when set.
    pub bookclerk_version: Option<String>,
    /// Liberated file / encoding version tag for `<file version>` when set.
    pub file_version: Option<String>,
}

impl BookContext {
    /// `<title>` value (full title with subtitle, or audible title).
    pub(crate) fn full_title(&self) -> Option<String> {
        self.title_with_subtitle
            .clone()
            .or_else(|| self.title.clone())
    }

    /// `<title short>`: audible title truncated at the first colon.
    pub(crate) fn title_short(&self) -> Option<String> {
        let title = self.title.as_deref()?;
        Some(match title.find(':') {
            Some(i) => title[..i].to_string(),
            None => title.to_string(),
        })
    }

    pub(crate) fn is_series(&self) -> bool {
        !self.series.is_empty()
    }

    pub(crate) fn is_podcast(&self) -> bool {
        matches!(
            self.content_kind,
            ContentKind::Podcast | ContentKind::PodcastParent | ContentKind::Episode
        )
    }

    pub(crate) fn is_podcast_parent(&self) -> bool {
        matches!(
            self.content_kind,
            ContentKind::PodcastParent | ContentKind::Podcast
        )
    }
}

/// Per-chapter context for chapter file / title templates.
#[derive(Debug, Clone, Default)]
pub struct ChapterContext {
    /// 1-based chapter position (`<ch#>`).
    pub chapter_number: u32,
    /// Total number of chapters (`<ch count>`).
    pub chapter_count: u32,
    /// Chapter title (`<ch title>`).
    pub chapter_title: Option<String>,
    /// Optional chapter file date override (`<file date>`).
    pub file_date: Option<NaiveDateTime>,
}
