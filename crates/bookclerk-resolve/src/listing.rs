//! The validated listing a storefront reports, and who holds it.
//!
//! [`ListingDraft`] is what a plugin said, unchecked. [`Listing::from_draft`]
//! is the boundary: it trims and normalises every field, drops blank
//! identifiers, credits and series, removes duplicates, and refuses records
//! that are empty or too large. The resolver only ever sees a [`Listing`].

use std::collections::BTreeSet;

use crate::identifiers::IdentifierKey;
use crate::kinds::{
    Abridgement, CreditRole, Entitlement, ExpressionKind, IdentifierScheme, Level, MediaKind,
    WorkKind,
};
use crate::normalize::clean_author;

/// The marketplace a listing that names none is sold in.
pub const DEFAULT_MARKETPLACE: &str = "us";

/// The stored spelling of a marketplace a source reported: trimmed,
/// lower-cased, and [`DEFAULT_MARKETPLACE`] when blank.
#[must_use]
pub fn stored_marketplace(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        DEFAULT_MARKETPLACE.to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

/// The stored spelling of a product id a source reported: its words
/// joined by single spaces, as a listing keeps it, so a title row holding
/// the raw value still finds its listing.
#[must_use]
pub fn stored_product_id(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Longest text field accepted, in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;
/// Most identifiers one listing may carry; keeps one listing's link
/// batch within every database engine's statement cap.
pub const MAX_IDENTIFIERS: usize = 12;
/// Most credits one listing may carry.
pub const MAX_CREDITS: usize = 128;
/// Most series entries one listing may carry; small enough that a work's
/// series and series entries are written in one credit batch, so they are
/// never left part-written.
pub const MAX_SERIES: usize = 12;

/// What a store sells, keyed the way `store_listings` is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListingKey {
    /// Provenance-qualified PluginKey of the storefront.
    pub plugin_key: String,
    /// Lower-cased marketplace (`us`, `uk`, …).
    pub marketplace: String,
    /// The store's own product id.
    pub product_id: String,
}

/// Who owns a listing (`holdings.holder`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Holder {
    /// A store account of the listing's plugin, by account id.
    Account(String),
    /// A branch library, by node id (`node:<node_id>` provenance).
    Node(String),
}

impl Holder {
    /// The stored text: `account:<id>` or `node:<id>`.
    #[must_use]
    pub fn stored(&self) -> String {
        match self {
            Self::Account(id) => format!("account:{id}"),
            Self::Node(id) => format!("node:{id}"),
        }
    }

    /// Parses stored holder text; `None` for any other shape.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(id) = text.strip_prefix("account:") {
            return (!id.is_empty()).then(|| Self::Account(id.to_string()));
        }
        if let Some(id) = text.strip_prefix("node:") {
            return (!id.is_empty()).then(|| Self::Node(id.to_string()));
        }
        None
    }
}

/// One credited contributor on a listing, with its resolved level.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Credit {
    /// Display name, whitespace collapsed.
    pub name: String,
    /// What the contributor did.
    pub role: CreditRole,
    /// The level the credit attaches to.
    pub level: Level,
}

impl Credit {
    /// The cleaned name contributors are matched by.
    #[must_use]
    pub fn name_key(&self) -> String {
        clean_author(&self.name)
    }
}

/// A work's place in one series, as a listing reports it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesEntry {
    /// Series name, whitespace collapsed.
    pub name: String,
    /// Position label (`1`, `2.5`, `1-3`) when known.
    pub position: Option<String>,
}

impl SeriesEntry {
    /// The cleaned name series are matched by.
    #[must_use]
    pub fn name_key(&self) -> String {
        crate::normalize::clean_title(&self.name, true)
    }

    /// The position as a sortable number: the leading decimal of the label.
    ///
    /// `1-3` sorts as 1 and `Book 2` as 2; a label without digits has none.
    #[must_use]
    pub fn sort_position(&self) -> Option<f64> {
        let label = self.position.as_deref()?;
        let start = label.find(|c: char| c.is_ascii_digit())?;
        let rest = &label[start..];
        let mut end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        // `2.` and `1.2.3` keep only the first well-formed decimal.
        let mut number = &rest[..end];
        if let Some(second) = number.match_indices('.').nth(1).map(|(i, _)| i) {
            end = second;
            number = &rest[..end];
        }
        number
            .trim_end_matches('.')
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
    }
}

/// An unchecked listing, field for field as a plugin reported it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListingDraft {
    /// PluginKey of the storefront; the host sets it, never the guest.
    pub plugin_key: String,
    /// Marketplace when known; blank reads as `us`.
    pub marketplace: Option<String>,
    /// The store's own product id.
    pub product_id: String,
    /// Kind of work.
    pub work_kind: Option<WorkKind>,
    /// How the work is realised.
    pub expression_kind: Option<ExpressionKind>,
    /// Carrier of the edition.
    pub media_kind: Option<MediaKind>,
    /// Content language code when known.
    pub language: Option<String>,
    /// Abridgement as reported.
    pub abridgement: Option<Abridgement>,
    /// Title.
    pub title: String,
    /// Subtitle when distinct from the title.
    pub subtitle: Option<String>,
    /// Publication date (RFC 3339) when known.
    pub published_at: Option<String>,
    /// Runtime in whole minutes when known.
    pub duration_minutes: Option<i64>,
    /// Category labels as one string when known.
    pub categories: Option<String>,
    /// Identifiers: scheme, raw value, and the marketplace the source gave.
    pub identifiers: Vec<(IdentifierScheme, String, Option<String>)>,
    /// Credits in billing order: name, role, and a level when the source
    /// departs from the role's usual one.
    pub credits: Vec<(String, CreditRole, Option<Level>)>,
    /// Series entries: name and position label.
    pub series: Vec<(String, Option<String>)>,
    /// Parent show's product id, for a podcast episode (a podcast that names
    /// its show).
    pub parent_product_id: Option<String>,
}

/// Why a draft listing was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingError {
    /// A required field (plugin key, product id or title) is blank.
    Blank(&'static str),
    /// A text field is longer than [`MAX_TEXT_BYTES`].
    TooLong(&'static str),
    /// A list holds more entries than its limit.
    TooMany(&'static str),
    /// The runtime is negative.
    NegativeDuration,
    /// A kind (work, expression or media) is absent: every record names
    /// all three, so a plugin that leaves one unspecified is refused.
    MissingKind(&'static str),
}

impl std::fmt::Display for ListingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blank(field) => write!(f, "listing field `{field}` is blank"),
            Self::TooLong(field) => {
                write!(f, "listing field `{field}` is over {MAX_TEXT_BYTES} bytes")
            }
            Self::TooMany(field) => write!(f, "listing has too many `{field}` entries"),
            Self::NegativeDuration => f.write_str("listing runtime is negative"),
            Self::MissingKind(field) => write!(f, "listing field `{field}` is unspecified"),
        }
    }
}

impl std::error::Error for ListingError {}

/// A validated, normalised listing: the resolver's only input record.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    /// The store, marketplace and product it is.
    pub key: ListingKey,
    /// Kind of work.
    pub work_kind: WorkKind,
    /// How the work is realised.
    pub expression_kind: ExpressionKind,
    /// Carrier of the edition.
    pub media_kind: MediaKind,
    /// Lower-cased language code; `None` when unknown.
    pub language: Option<String>,
    /// Abridgement; [`Abridgement::Unknown`] when the source did not say.
    pub abridgement: Abridgement,
    /// Title, whitespace collapsed.
    pub title: String,
    /// Subtitle, whitespace collapsed.
    pub subtitle: Option<String>,
    /// Publication date (RFC 3339) when known.
    pub published_at: Option<String>,
    /// Runtime in whole minutes when known.
    pub duration_minutes: Option<i64>,
    /// Category labels when known.
    pub categories: Option<String>,
    /// Normalised identifiers, duplicates removed, in reported order.
    pub identifiers: Vec<IdentifierKey>,
    /// Credits in billing order, duplicates removed.
    pub credits: Vec<Credit>,
    /// Series entries, duplicates removed.
    pub series: Vec<SeriesEntry>,
    /// The parent show's product id, for a podcast episode only.
    pub parent_product_id: Option<String>,
}

/// Trims a text field, collapses its whitespace, and checks its length.
fn clean_text(raw: &str, field: &'static str) -> Result<String, ListingError> {
    if raw.len() > MAX_TEXT_BYTES {
        return Err(ListingError::TooLong(field));
    }
    Ok(raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Like [`clean_text`], answering `None` for a blank value.
fn clean_optional(raw: Option<&str>, field: &'static str) -> Result<Option<String>, ListingError> {
    match raw {
        Some(raw) => Ok(Some(clean_text(raw, field)?).filter(|s| !s.is_empty())),
        None => Ok(None),
    }
}

impl Listing {
    /// Validates and normalises a draft.
    ///
    /// # Arguments
    ///
    /// * `draft` - The listing as the plugin reported it.
    ///
    /// # Returns
    ///
    /// The normalised listing. A podcast keeps its parent id (an episode
    /// names its show); a book drops it.
    ///
    /// # Errors
    ///
    /// Returns [`ListingError`] when a kind is absent, the plugin key,
    /// product id or title is blank, a text field is too long, or a list is
    /// over its limit.
    pub fn from_draft(draft: &ListingDraft) -> Result<Self, ListingError> {
        let plugin_key = clean_text(&draft.plugin_key, "plugin_key")?;
        if plugin_key.is_empty() {
            return Err(ListingError::Blank("plugin_key"));
        }
        let product_id = clean_text(&draft.product_id, "product_id")?;
        if product_id.is_empty() {
            return Err(ListingError::Blank("product_id"));
        }
        let title = clean_text(&draft.title, "title")?;
        if title.is_empty() {
            return Err(ListingError::Blank("title"));
        }
        let marketplace = clean_optional(draft.marketplace.as_deref(), "marketplace")?
            .unwrap_or_else(|| DEFAULT_MARKETPLACE.to_string())
            .to_ascii_lowercase();
        if draft.identifiers.len() > MAX_IDENTIFIERS {
            return Err(ListingError::TooMany("identifiers"));
        }
        if draft.credits.len() > MAX_CREDITS {
            return Err(ListingError::TooMany("credits"));
        }
        if draft.series.len() > MAX_SERIES {
            return Err(ListingError::TooMany("series"));
        }
        if draft.duration_minutes.is_some_and(|m| m < 0) {
            return Err(ListingError::NegativeDuration);
        }

        let mut seen_ids = BTreeSet::new();
        let mut identifiers = Vec::new();
        for (scheme, raw, scope) in &draft.identifiers {
            if raw.len() > MAX_TEXT_BYTES
                || scope.as_ref().is_some_and(|s| s.len() > MAX_TEXT_BYTES)
            {
                return Err(ListingError::TooLong("identifiers"));
            }
            let Some(key) = IdentifierKey::new(*scheme, raw, scope.as_deref(), &marketplace) else {
                continue;
            };
            if seen_ids.insert(key.clone()) {
                identifiers.push(key);
            }
        }

        let mut seen_credits = BTreeSet::new();
        let mut credits = Vec::new();
        for (name, role, level) in &draft.credits {
            let name = clean_text(name, "credits")?;
            if name.is_empty() {
                continue;
            }
            let credit = Credit {
                name,
                role: *role,
                level: level.unwrap_or_else(|| role.default_level()),
            };
            if credit.name_key().is_empty() {
                continue;
            }
            if seen_credits.insert((credit.name_key(), credit.role, credit.level)) {
                credits.push(credit);
            }
        }

        let mut seen_series = BTreeSet::new();
        let mut series = Vec::new();
        for (name, position) in &draft.series {
            let entry = SeriesEntry {
                name: clean_text(name, "series")?,
                position: clean_optional(position.as_deref(), "series")?,
            };
            if entry.name_key().is_empty() {
                continue;
            }
            if seen_series.insert(entry.name_key()) {
                series.push(entry);
            }
        }

        let work_kind = draft
            .work_kind
            .ok_or(ListingError::MissingKind("work_kind"))?;
        let expression_kind = draft
            .expression_kind
            .ok_or(ListingError::MissingKind("expression_kind"))?;
        let media_kind = draft
            .media_kind
            .ok_or(ListingError::MissingKind("media_kind"))?;
        let parent_product_id = match work_kind {
            WorkKind::Podcast => {
                clean_optional(draft.parent_product_id.as_deref(), "parent_product_id")?
            }
            WorkKind::Book => None,
        };
        Ok(Self {
            key: ListingKey {
                plugin_key,
                marketplace,
                product_id,
            },
            work_kind,
            expression_kind,
            media_kind,
            language: clean_optional(draft.language.as_deref(), "language")?
                .map(|l| l.to_ascii_lowercase()),
            abridgement: draft.abridgement.unwrap_or(Abridgement::Unknown),
            subtitle: clean_optional(draft.subtitle.as_deref(), "subtitle")?,
            published_at: clean_optional(draft.published_at.as_deref(), "published_at")?,
            duration_minutes: draft.duration_minutes,
            categories: clean_optional(draft.categories.as_deref(), "categories")?,
            title,
            identifiers,
            credits,
            series,
            parent_product_id,
        })
    }

    /// Whether this listing is a podcast episode: a podcast that names its
    /// show. A podcast without a parent is the show.
    #[must_use]
    pub fn is_episode(&self) -> bool {
        self.work_kind == WorkKind::Podcast && self.parent_product_id.is_some()
    }

    /// The listing of this one's parent show, for an episode with a parent id.
    #[must_use]
    pub fn parent_key(&self) -> Option<ListingKey> {
        self.parent_product_id
            .as_ref()
            .map(|product_id| ListingKey {
                plugin_key: self.key.plugin_key.clone(),
                marketplace: self.key.marketplace.clone(),
                product_id: product_id.clone(),
            })
    }

    /// Credits attached at `level`, in billing order.
    pub fn credits_at(&self, level: Level) -> impl Iterator<Item = &Credit> {
        self.credits.iter().filter(move |c| c.level == level)
    }

    /// Identifiers whose scheme names an edition, in reported order.
    pub fn edition_identifiers(&self) -> impl Iterator<Item = &IdentifierKey> {
        self.identifiers
            .iter()
            .filter(|id| id.level() == Level::Edition)
    }

    /// Identifiers whose scheme names a work, in reported order.
    pub fn work_identifiers(&self) -> impl Iterator<Item = &IdentifierKey> {
        self.identifiers
            .iter()
            .filter(|id| id.level() == Level::Work)
    }

    /// The work matcher's key: cleaned title and sorted author names.
    ///
    /// Empty when the cleaned title is empty or no author is credited at
    /// the work level, so such a listing is never matched by text alone.
    #[must_use]
    pub fn work_match_key(&self) -> String {
        work_match_key(
            &self.title,
            self.credits_at(Level::Work)
                .filter(|c| c.role == CreditRole::Author)
                .map(|c| c.name.as_str()),
        )
    }

    /// The expression's credit key: its credits as sorted, cleaned
    /// `role:name` pairs, so two realisations whose people swap roles
    /// (narrator and translator, say) never compare equal.
    #[must_use]
    pub fn expression_credit_key(&self) -> String {
        let set: BTreeSet<String> = self
            .credits_at(Level::Expression)
            .filter_map(|c| {
                let name = clean_author(&c.name);
                (!name.is_empty()).then(|| format!("{}:{name}", c.role.as_str()))
            })
            .collect();
        set.into_iter().collect::<Vec<_>>().join("\u{1e}")
    }
}

/// The work matcher's key for a title and its authors.
///
/// # Arguments
///
/// * `title` - Work title (decoded, any case).
/// * `authors` - Author display names.
///
/// # Returns
///
/// `clean_title` and the sorted, de-duplicated `clean_author` names joined
/// with unit separators; empty when either side is empty.
#[must_use]
pub fn work_match_key<'a>(title: &str, authors: impl Iterator<Item = &'a str>) -> String {
    let title = crate::normalize::clean_title(title, true);
    let authors = name_set_key(authors);
    if title.is_empty() || authors.is_empty() {
        return String::new();
    }
    format!("{title}\u{1f}{authors}")
}

/// Sorted, de-duplicated cleaned names joined with record separators.
fn name_set_key<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let set: BTreeSet<String> = names.map(clean_author).filter(|n| !n.is_empty()).collect();
    set.into_iter().collect::<Vec<_>>().join("\u{1e}")
}

/// Ownership details a scan reports with a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldingDraft {
    /// Who holds the listing.
    pub holder: Holder,
    /// How they hold it.
    pub entitlement: Entitlement,
    /// When they bought or added it (RFC 3339), when known.
    pub purchased_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> ListingDraft {
        ListingDraft {
            plugin_key: "audible".into(),
            product_id: " B001 ".into(),
            title: "  Dune ".into(),
            credits: vec![
                ("Frank Herbert".into(), CreditRole::Author, None),
                ("Frank  Herbert".into(), CreditRole::Author, None),
                ("Scott Brick".into(), CreditRole::Narrator, None),
                ("  ".into(), CreditRole::Narrator, None),
            ],
            identifiers: vec![
                (IdentifierScheme::Asin, "b001".into(), None),
                (IdentifierScheme::Asin, "B001".into(), Some("US".into())),
                (IdentifierScheme::Isbn, " ".into(), None),
            ],
            series: vec![("Dune".into(), Some("1".into())), ("dune".into(), None)],
            parent_product_id: Some("P1".into()),
            work_kind: Some(WorkKind::Book),
            expression_kind: Some(ExpressionKind::Narration),
            media_kind: Some(MediaKind::Audio),
            ..ListingDraft::default()
        }
    }

    #[test]
    fn drafts_normalise_and_deduplicate() {
        let l = Listing::from_draft(&draft()).unwrap();
        assert_eq!(l.key.product_id, "B001");
        assert_eq!(l.key.marketplace, "us");
        assert_eq!(l.title, "Dune");
        assert_eq!(l.identifiers.len(), 1);
        assert_eq!(l.credits.len(), 2);
        assert_eq!(l.credits[1].level, Level::Expression);
        assert_eq!(l.series.len(), 1);
        // Only an episode keeps its parent.
        assert_eq!(l.parent_product_id, None);
        assert_eq!(l.work_match_key(), "dune\u{1f}frank herbert");
        assert_eq!(l.expression_credit_key(), "narrator:scott brick");
    }

    #[test]
    fn episodes_keep_their_parent() {
        let mut d = draft();
        d.work_kind = Some(WorkKind::Podcast);
        let l = Listing::from_draft(&d).unwrap();
        assert!(l.is_episode());
        assert_eq!(l.parent_key().unwrap().product_id, "P1");
        d.parent_product_id = None;
        assert!(!Listing::from_draft(&d).unwrap().is_episode());
    }

    #[test]
    fn drafts_without_a_kind_are_refused() {
        for (field, clear) in [
            (
                "work_kind",
                (|d: &mut ListingDraft| d.work_kind = None) as fn(&mut ListingDraft),
            ),
            ("expression_kind", |d| d.expression_kind = None),
            ("media_kind", |d| d.media_kind = None),
        ] {
            let mut d = draft();
            clear(&mut d);
            assert_eq!(
                Listing::from_draft(&d),
                Err(ListingError::MissingKind(field))
            );
        }
    }

    #[test]
    fn bad_drafts_are_refused() {
        let mut d = draft();
        d.title = " ".into();
        assert_eq!(Listing::from_draft(&d), Err(ListingError::Blank("title")));
        let mut d = draft();
        d.product_id.clear();
        assert_eq!(
            Listing::from_draft(&d),
            Err(ListingError::Blank("product_id"))
        );
        let mut d = draft();
        d.plugin_key.clear();
        assert_eq!(
            Listing::from_draft(&d),
            Err(ListingError::Blank("plugin_key"))
        );
        let mut d = draft();
        d.subtitle = Some("x".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(
            Listing::from_draft(&d),
            Err(ListingError::TooLong("subtitle"))
        );
        let mut d = draft();
        d.identifiers = vec![(IdentifierScheme::Isbn, "1".into(), None); MAX_IDENTIFIERS + 1];
        assert_eq!(
            Listing::from_draft(&d),
            Err(ListingError::TooMany("identifiers"))
        );
        let mut d = draft();
        d.identifiers = vec![(
            IdentifierScheme::Asin,
            "B001".into(),
            Some("x".repeat(MAX_TEXT_BYTES + 1)),
        )];
        assert_eq!(
            Listing::from_draft(&d),
            Err(ListingError::TooLong("identifiers"))
        );
        let mut d = draft();
        d.duration_minutes = Some(-1);
        assert_eq!(Listing::from_draft(&d), Err(ListingError::NegativeDuration));
        assert!(ListingError::NegativeDuration
            .to_string()
            .contains("negative"));
    }

    #[test]
    fn match_keys_need_a_title_and_an_author() {
        let mut d = draft();
        d.credits.retain(|(_, role, _)| *role != CreditRole::Author);
        assert_eq!(Listing::from_draft(&d).unwrap().work_match_key(), "");
        assert_eq!(work_match_key("(Unabridged)", ["A"].into_iter()), "");
    }

    #[test]
    fn holders_round_trip() {
        for h in [Holder::Account("a1".into()), Holder::Node("n1".into())] {
            assert_eq!(Holder::parse(&h.stored()), Some(h));
        }
        assert_eq!(Holder::parse("account:"), None);
        assert_eq!(Holder::parse("user:1"), None);
    }

    #[test]
    fn series_positions_sort_by_their_leading_number() {
        let pos = |p: &str| {
            SeriesEntry {
                name: "S".into(),
                position: Some(p.into()),
            }
            .sort_position()
        };
        assert_eq!(pos("1"), Some(1.0));
        assert_eq!(pos("2.5"), Some(2.5));
        assert_eq!(pos("1-3"), Some(1.0));
        assert_eq!(pos("Book 2"), Some(2.0));
        assert_eq!(pos("1.2.3"), Some(1.2));
        assert_eq!(pos("2."), Some(2.0));
        assert_eq!(pos("prequel"), None);
        assert_eq!(pos(&"9".repeat(400)), None);
    }
}
