//! Stored rows the resolver reads, the snapshot of them it is given, and
//! the plan of writes it returns.

use crate::identifiers::IdentifierKey;
use crate::kinds::{Abridgement, Entitlement, ExpressionKind, Level, MediaKind, WorkKind};
use crate::listing::{Credit, Holder, ListingKey, SeriesEntry};

/// A stored work (`works`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRow {
    /// Work id.
    pub id: String,
    /// Kind of work.
    pub kind: WorkKind,
    /// Display title.
    pub title: String,
    /// The work matcher's key ([`crate::listing::work_match_key`]); empty
    /// when the work is never matched by text.
    pub match_key: String,
    /// The show a podcast episode belongs to.
    pub parent_work_id: Option<String>,
}

/// A stored expression (`expressions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionRow {
    /// Expression id.
    pub id: String,
    /// The work it realises.
    pub work_id: String,
    /// How it realises the work.
    pub kind: ExpressionKind,
    /// Lower-cased language code; `None` when unknown.
    pub language: Option<String>,
    /// Abridgement.
    pub abridgement: Abridgement,
    /// Sorted, cleaned names of its narrator, translator, host and cast
    /// credits ([`crate::listing::Listing::expression_credit_key`]).
    pub credit_key: String,
}

/// A stored edition (`editions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionRow {
    /// Edition id.
    pub id: String,
    /// The expression it releases.
    pub expression_id: String,
    /// Its form.
    pub media_kind: MediaKind,
    /// Display title.
    pub title: String,
    /// Subtitle.
    pub subtitle: Option<String>,
    /// Publication date (RFC 3339).
    pub published_at: Option<String>,
    /// Runtime in whole minutes.
    pub duration_minutes: Option<i64>,
}

/// An identifier attached to an edition (`edition_identifiers`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EditionIdentifierRow {
    /// The edition.
    pub edition_id: String,
    /// The identifier.
    pub key: IdentifierKey,
}

/// An identifier attached to a work (`work_identifiers`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkIdentifierRow {
    /// The work.
    pub work_id: String,
    /// The identifier.
    pub key: IdentifierKey,
}

/// A stored store listing (`store_listings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingRow {
    /// Listing id.
    pub id: String,
    /// Store, marketplace and product.
    pub key: ListingKey,
    /// The edition it sells.
    pub edition_id: String,
}

/// A stored holding (`holdings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldingRow {
    /// The listing held.
    pub listing_id: String,
    /// Who holds it.
    pub holder: Holder,
    /// How they hold it.
    pub entitlement: Entitlement,
    /// When they bought or added it (RFC 3339).
    pub purchased_at: Option<String>,
}

/// An Administrator's decision the resolver obeys (`resolution_overrides`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Override {
    /// This listing sells that edition, whatever its identifiers say.
    PinListing {
        /// The listing.
        listing: ListingKey,
        /// The edition it sells.
        edition_id: String,
    },
    /// The work matcher never joins a new listing to this work (a split).
    IsolateWork {
        /// The work.
        work_id: String,
    },
}

/// The stored rows that bear on one listing, read before resolving it.
///
/// The caller fills it with:
///
/// - the listing's own row and its parent show's;
/// - every edition identifier row sharing one of the listing's edition
///   identifiers, and every identifier row of each edition included;
/// - every work identifier row sharing one of the listing's work identifiers;
/// - the editions those rows, the two listings and any pin reach, with
///   their expressions and works;
/// - every work with the listing's match key and work kind;
/// - every expression of each included work;
/// - the pins on the listing and the isolations of included works;
/// - the holder's holding of the listing;
/// - which included works, expressions and editions already have credits,
///   and which included works already have series entries.
///
/// [`Snapshot::is_closed`] checks the references are complete.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The listing's stored row, when it was seen before.
    pub listing: Option<ListingRow>,
    /// The parent show's stored listing, for an episode.
    pub parent_listing: Option<ListingRow>,
    /// Stored rows sharing one of the listing's edition identifiers.
    pub edition_identifiers: Vec<EditionIdentifierRow>,
    /// Stored rows sharing one of the listing's work identifiers.
    pub work_identifiers: Vec<WorkIdentifierRow>,
    /// Editions the rows above reach.
    pub editions: Vec<EditionRow>,
    /// Expressions of those editions and of every included work.
    pub expressions: Vec<ExpressionRow>,
    /// Works of those expressions, identifier rows and match candidates.
    pub works: Vec<WorkRow>,
    /// Overrides on the listing or on any included work.
    pub overrides: Vec<Override>,
    /// The holder's stored holding of the listing.
    pub holding: Option<HoldingRow>,
    /// Included works, expressions and editions that have at least one credit.
    pub credited: Vec<(Level, String)>,
    /// Included works that have at least one series entry.
    pub works_with_series: Vec<String>,
}

impl Snapshot {
    /// The edition with `id`, when included.
    #[must_use]
    pub fn edition(&self, id: &str) -> Option<&EditionRow> {
        self.editions.iter().find(|e| e.id == id)
    }

    /// The expression with `id`, when included.
    #[must_use]
    pub fn expression(&self, id: &str) -> Option<&ExpressionRow> {
        self.expressions.iter().find(|e| e.id == id)
    }

    /// The work with `id`, when included.
    #[must_use]
    pub fn work(&self, id: &str) -> Option<&WorkRow> {
        self.works.iter().find(|w| w.id == id)
    }

    /// The work an included edition realises, through its expression.
    #[must_use]
    pub fn work_of_edition(&self, edition_id: &str) -> Option<&WorkRow> {
        let edition = self.edition(edition_id)?;
        let expression = self.expression(&edition.expression_id)?;
        self.work(&expression.work_id)
    }

    /// Checks that every edition and work the snapshot refers to is
    /// included, with each edition's expression and work.
    ///
    /// # Returns
    ///
    /// The first missing reference, described, or `None` when closed.
    #[must_use]
    pub fn is_closed(&self) -> Option<String> {
        let edition_ids = self
            .listing
            .iter()
            .chain(self.parent_listing.iter())
            .map(|l| l.edition_id.as_str())
            .chain(
                self.edition_identifiers
                    .iter()
                    .map(|r| r.edition_id.as_str()),
            )
            .chain(self.overrides.iter().filter_map(|o| match o {
                Override::PinListing { edition_id, .. } => Some(edition_id.as_str()),
                Override::IsolateWork { .. } => None,
            }));
        for id in edition_ids {
            let Some(edition) = self.edition(id) else {
                return Some(format!("edition {id}"));
            };
            let Some(expression) = self.expression(&edition.expression_id) else {
                return Some(format!("expression {}", edition.expression_id));
            };
            if self.work(&expression.work_id).is_none() {
                return Some(format!("work {}", expression.work_id));
            }
        }
        for row in &self.work_identifiers {
            if self.work(&row.work_id).is_none() {
                return Some(format!("work {}", row.work_id));
            }
        }
        None
    }
}

/// Why a listing reached the edition it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkReason {
    /// An Administrator pinned the listing to the edition.
    Pinned,
    /// The listing was stored before and keeps its edition.
    Listing,
    /// A strong identifier named the edition.
    StrongIdentifier,
    /// A weak identifier proposed the edition and its expression and
    /// media kind were compatible.
    WeakIdentifier,
    /// No stored edition matched; a new one is created.
    New,
}

/// How a new edition found its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkReason {
    /// A shared work identifier named the work.
    Identifier,
    /// The work matcher found exactly one work.
    Matcher,
    /// No work matched; a new one is created.
    New,
}

/// Something the resolver could not decide alone; recorded for an
/// Administrator (`resolution_conflicts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// The listing's strong identifiers name more than one edition, or an
    /// edition other than the one it reached. None of them were merged.
    StrongIdentifiers {
        /// The editions the identifiers name, the reached one first.
        edition_ids: Vec<String>,
    },
    /// The listing's work identifiers name more than one work, or a work
    /// other than its edition's.
    WorkIdentifiers {
        /// The works the identifiers name, the chosen one first.
        work_ids: Vec<String>,
    },
    /// More than one stored work matches the listing's title and authors,
    /// so it joined none of them.
    AmbiguousWork {
        /// The matching works.
        work_ids: Vec<String>,
    },
    /// A work identifier names a work of a different kind; it was not joined.
    WorkKindMismatch {
        /// The work the identifier names.
        work_id: String,
    },
}

/// A `core.events` outbox event the link publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A listing joined an edition, new or existing (`edition_linked`).
    EditionLinked {
        /// The edition.
        edition_id: String,
        /// The listing.
        listing_id: String,
    },
    /// A rescan changed an existing edition's metadata (`edition_updated`).
    EditionUpdated {
        /// The edition.
        edition_id: String,
    },
    /// A holder gained a listing (`holding_added`).
    HoldingAdded {
        /// The listing.
        listing_id: String,
        /// The holder.
        holder: Holder,
    },
}

impl Event {
    /// The event type written to the outbox.
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::EditionLinked { .. } => "edition_linked",
            Self::EditionUpdated { .. } => "edition_updated",
            Self::HoldingAdded { .. } => "holding_added",
        }
    }
}

/// A credit to write on a new work, expression or edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditWrite {
    /// The level of the target.
    pub level: Level,
    /// The target's id.
    pub target_id: String,
    /// The credit.
    pub credit: Credit,
    /// Billing position, from 0.
    pub position: u32,
}

/// A series entry to write on a new work.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesWrite {
    /// The work.
    pub work_id: String,
    /// The entry.
    pub entry: SeriesEntry,
}

/// Every write one listing's link needs, applied as one atomic batch.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The listing row to insert or refresh.
    pub listing: ListingRow,
    /// True when the listing row is new.
    pub listing_is_new: bool,
    /// Why the listing reached its edition.
    pub link: LinkReason,
    /// How a new edition found its work; `None` for an existing edition.
    pub work_reason: Option<WorkReason>,
    /// A new work to insert.
    pub new_work: Option<WorkRow>,
    /// A new expression to insert.
    pub new_expression: Option<ExpressionRow>,
    /// A new edition to insert.
    pub new_edition: Option<EditionRow>,
    /// New metadata for an existing edition a rescan changed.
    pub edition_update: Option<EditionRow>,
    /// Edition identifiers to attach.
    pub edition_identifiers: Vec<EditionIdentifierRow>,
    /// Work identifiers to attach.
    pub work_identifiers: Vec<WorkIdentifierRow>,
    /// Credits for the work, expression and edition the listing reached,
    /// for each of them that has none yet.
    pub credits: Vec<CreditWrite>,
    /// Series entries for the work, when it has none yet.
    pub series: Vec<SeriesWrite>,
    /// The holding to insert or refresh.
    pub holding: HoldingRow,
    /// True when the holding is new.
    pub holding_is_new: bool,
    /// Decisions left to an Administrator.
    pub conflicts: Vec<Conflict>,
    /// Events to publish.
    pub events: Vec<Event>,
}

impl Plan {
    /// True when applying the plan would change nothing but timestamps:
    /// no new rows, no metadata change, no conflict and no event.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        !self.listing_is_new
            && !self.holding_is_new
            && self.new_work.is_none()
            && self.new_expression.is_none()
            && self.new_edition.is_none()
            && self.edition_update.is_none()
            && self.edition_identifiers.is_empty()
            && self.work_identifiers.is_empty()
            && self.credits.is_empty()
            && self.series.is_empty()
            && self.events.is_empty()
    }
}
