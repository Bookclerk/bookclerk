//! SeaORM entity for `destination_feeds`: how each output destination's
//! outside changes are followed, and how healthy that is (storage
//! destinations ADR, `docs/adr/storage-destinations.md`, decision 5).
//!
//! One row per destination, written by the destination sync pass that
//! holds the destination's lease ([`crate::LibraryStore::save_destination_feed`])
//! and read for its next pass and for the destination's health in Settings.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "destination_feeds")]
pub struct Model {
    /// The destination's plugin instance id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// The last change sequence number handed out for this destination.
    pub seq: i64,
    /// The change feed's state (`FeedState::as_str`): `off`, `healthy`,
    /// `gap` or `failing`.
    pub feed_state: String,
    /// The feed cursor to read from next; `None` before the first read.
    pub feed_cursor: Option<String>,
    /// The last feed failure, for operators.
    pub feed_error: Option<String>,
    /// RFC 3339 time the feed was last read.
    pub feed_checked_at: Option<String>,
    /// Sequence of the listing in progress; `None` between listings.
    pub listing_seq: Option<i64>,
    /// Cursor of the next page of the listing in progress.
    pub listing_cursor: Option<String>,
    /// RFC 3339 time the listing in progress (or the last one) started.
    pub listing_started_at: Option<String>,
    /// RFC 3339 time the last listing finished.
    pub listed_at: Option<String>,
    /// RFC 3339 time the next scheduled listing is due.
    pub next_listing_at: Option<String>,
    /// Why the last listing failed or was not applied, for operators.
    pub listing_error: Option<String>,
    /// Objects the destination held at the end of the last listing or feed
    /// page.
    pub objects: i64,
    /// The last sequence the copy check has read changes up to.
    pub checked_seq: i64,
    /// Objects known when the listing in progress started, which its
    /// removal guard is judged against; `None` between listings.
    pub listing_base: Option<i64>,
    /// Write count of this row: each save names the revision it read and
    /// fails when another writer saved since (a sync pass whose lease
    /// another node took).
    pub revision: i64,
    /// RFC 3339 time the row was last written.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
