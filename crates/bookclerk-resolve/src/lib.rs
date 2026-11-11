//! Sans-IO resolver for the library data model.
//!
//! Storefronts describe what they sell; the host decides identity. This
//! crate maps one store listing to an FRBR edition, expression and work
//! ([library data model ADR](https://github.com/Bookclerk/bookclerk/blob/main/docs/adr/library-data-model.md)):
//!
//! 1. [`Listing::from_draft`] validates and normalises what a plugin
//!    reported. It is the untrusted-input boundary.
//! 2. The caller reads a [`Snapshot`] of the stored rows that bear on the
//!    listing (the store from SQL, [`memory::MemoryCatalog`] from memory).
//! 3. [`resolve`] returns a [`Plan`]: the rows to insert, the identifiers to
//!    attach, the conflicts to record for an Administrator and the events
//!    to publish. The caller applies it as one atomic batch.
//!
//! Nothing here performs I/O, so the fuzz target (`library_resolve`), the
//! replay test and the store all drive the same code.

pub mod fuzzing;
pub mod identifiers;
pub mod kinds;
pub mod listing;
pub mod memory;
pub mod model;
pub mod names;
pub mod normalize;
mod resolve;

pub use identifiers::{normalize_identifier, IdentifierKey};
pub use kinds::{
    Abridgement, CreditRole, Entitlement, ExpressionKind, IdentifierScheme, Level, MediaKind,
    Strength, WorkKind,
};
pub use listing::{
    stored_marketplace, stored_product_id, work_match_key, Credit, Holder, HoldingDraft, Listing,
    ListingDraft, ListingError, ListingKey, SeriesEntry,
};
pub use model::{
    Conflict, CreditWrite, EditionIdentifierRow, EditionRow, Event, ExpressionRow, HoldingRow,
    LinkReason, ListingRow, Override, Plan, SeriesWrite, Snapshot, WorkIdentifierRow, WorkMatchKey,
    WorkParent, WorkReason, WorkRow,
};
pub use resolve::{resolve, resolve_catalog, EntityKind, IdSource};
