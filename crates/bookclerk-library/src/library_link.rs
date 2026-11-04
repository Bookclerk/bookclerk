//! Named-atomic parameters for writing a resolved listing.
//!
//! [`bookclerk_resolve::resolve`] decides what a listing links to; these
//! types carry its plan to the database as host-compiled SQL batches
//! ([`crate::DbAtomicParams::LinkListing`] and
//! [`crate::DbAtomicParams::WriteCredits`]). The link is one atomic batch,
//! never a read-modify-write across statements, because D1 has no
//! interactive transactions: every insert is guarded in its own `WHERE`
//! clause, and the batch answers `retry` when another writer got there
//! first, so the caller resolves again against the new rows.

use serde::{Deserialize, Serialize};

/// A new `works` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkWorkRow {
    /// Work id.
    pub id: String,
    /// Stored work kind (`book`, …).
    pub kind: String,
    /// Display title.
    pub title: String,
    /// The work matcher's key; empty when never matched by text.
    pub match_key: String,
    /// Parent show, for a podcast episode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_work_id: Option<String>,
}

/// A new `expressions` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkExpressionRow {
    /// Expression id.
    pub id: String,
    /// The work it realises.
    pub work_id: String,
    /// Stored expression kind.
    pub kind: String,
    /// Lower-cased language code when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Stored abridgement.
    pub abridgement: String,
    /// Sorted, cleaned performer names.
    pub credit_key: String,
}

/// A new `editions` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkEditionRow {
    /// Edition id.
    pub id: String,
    /// The expression it releases.
    pub expression_id: String,
    /// Stored media kind.
    pub media_kind: String,
    /// Display title.
    pub title: String,
    /// Subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Publication date (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Runtime in whole minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
}

/// The show a stored, show-less episode work gains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkWorkParent {
    /// The episode work.
    pub work_id: String,
    /// The show work.
    pub parent_work_id: String,
}

/// The matcher key a stored work without one gains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkWorkMatchKey {
    /// The work, whose stored key is empty.
    pub work_id: String,
    /// Its stored kind; only works of this kind compete for the key.
    pub kind: String,
    /// Its show, for an episode; episodes only compete within one show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_work_id: Option<String>,
    /// The key it gains.
    pub match_key: String,
}

/// Fields a listing fills on a stored edition that lacks them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkEditionFill {
    /// Subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Publication date (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Runtime in whole minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
}

/// An `edition_identifiers` row to attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkIdentifierRow {
    /// Stored scheme.
    pub scheme: String,
    /// Normalised value.
    pub value: String,
    /// Lower-cased marketplace for a scoped scheme; empty otherwise.
    pub marketplace: String,
    /// `strong` or `weak`.
    pub strength: String,
    /// Unique key of a strong identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strong_key: Option<String>,
}

/// A `work_identifiers` row to attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkWorkIdentifierRow {
    /// Stored scheme.
    pub scheme: String,
    /// Normalised value.
    pub value: String,
}

/// A `resolution_conflicts` row to record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkConflictRow {
    /// Stored conflict kind (`strong_identifiers`, …).
    pub kind: String,
    /// JSON array of the edition or work ids involved.
    pub subject_ids: String,
}

/// A `core.events` row the link publishes when it commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkEvent {
    /// Event id (UUID).
    pub id: String,
    /// Event type (`edition_linked`, `edition_updated`, `holding_added`).
    pub event_type: String,
    /// Tenant account id; empty for library-wide events.
    pub account_id: String,
    /// PluginKey of the storefront that reported the listing.
    pub source: String,
    /// Unique with the event type; a replay never publishes twice.
    pub dedup_key: String,
    /// Bounded JSON payload.
    pub payload: String,
    /// FIFO key copied onto deliveries.
    pub ordering_key: String,
}

/// Everything one listing's link writes, as one atomic batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingLinkParams {
    /// Listing id (new or stored).
    pub listing_id: String,
    /// True when the batch inserts the listing.
    pub listing_is_new: bool,
    /// Storefront PluginKey.
    pub plugin_key: String,
    /// Lower-cased marketplace.
    pub marketplace: String,
    /// The store's product id.
    pub product_id: String,
    /// Title as the store lists it.
    pub title: String,
    /// Store category labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<String>,
    /// The edition the listing reaches.
    pub edition_id: String,
    /// The edition a stored listing sold when the plan was made; the
    /// refresh only applies while it still does, so a concurrent relink
    /// makes the batch answer `retry` instead of being overwritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_edition_id: Option<String>,
    /// Strong keys that must still be unowned for a new edition to be
    /// created; another writer taking one makes the batch answer `retry`.
    #[serde(default)]
    pub gate_strong_keys: Vec<String>,
    /// A new work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_work: Option<LinkWorkRow>,
    /// A new expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_expression: Option<LinkExpressionRow>,
    /// A new edition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_edition: Option<LinkEditionRow>,
    /// Fields to fill on a stored edition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition_fill: Option<LinkEditionFill>,
    /// A show link for a stored episode work that has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_parent: Option<LinkWorkParent>,
    /// Works with the new work's matcher key that the snapshot already
    /// held; any other same-key work stored since holds the new work back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_rivals: Vec<String>,
    /// A matcher key for a stored work that has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_match_key: Option<LinkWorkMatchKey>,
    /// Edition identifiers to attach.
    #[serde(default)]
    pub edition_identifiers: Vec<LinkIdentifierRow>,
    /// The work work identifiers attach to.
    #[serde(default)]
    pub work_id: String,
    /// Work identifiers to attach.
    #[serde(default)]
    pub work_identifiers: Vec<LinkWorkIdentifierRow>,
    /// Stored holder (`account:<id>` or `node:<id>`).
    pub holder: String,
    /// Stored entitlement.
    pub entitlement: String,
    /// When the holder bought or added it (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purchased_at: Option<String>,
    /// Conflicts to record.
    #[serde(default)]
    pub conflicts: Vec<LinkConflictRow>,
    /// Events to publish.
    #[serde(default)]
    pub events: Vec<LinkEvent>,
}

/// A `contributors` row (id derived from the cleaned name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContributorRow {
    /// Contributor id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Name for sorting.
    pub sort_name: String,
    /// Cleaned name; unique.
    pub name_key: String,
}

/// A `credits` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditRow {
    /// `work`, `expression` or `edition`.
    pub target_kind: String,
    /// Target id.
    pub target_id: String,
    /// Contributor id.
    pub contributor_id: String,
    /// Stored role.
    pub role: String,
    /// Billing position.
    pub position: i64,
}

/// A `series` row (id derived from the cleaned name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesRow {
    /// Series id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Cleaned name; unique.
    pub name_key: String,
}

/// A `series_entries` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesEntryRow {
    /// Series id.
    pub series_id: String,
    /// Work id.
    pub work_id: String,
    /// Position label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// Sortable position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_position: Option<f64>,
}

/// One batch of idempotent credit and series inserts.
///
/// Contributors and series precede the rows that reference them, in this
/// batch or an earlier one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditBatchParams {
    /// Contributors to insert when missing.
    #[serde(default)]
    pub contributors: Vec<ContributorRow>,
    /// Credits to insert when missing.
    #[serde(default)]
    pub credits: Vec<CreditRow>,
    /// Series to insert when missing.
    #[serde(default)]
    pub series: Vec<SeriesRow>,
    /// Series entries to insert when missing.
    #[serde(default)]
    pub series_entries: Vec<SeriesEntryRow>,
}

impl CreditBatchParams {
    /// The number of insert statements the batch compiles to.
    #[must_use]
    pub fn statement_count(&self) -> usize {
        self.contributors.len() + self.credits.len() + self.series.len() + self.series_entries.len()
    }
}
