//! Canonical Bookclerk library database (SQLite via rusqlite).

mod error;
mod migrations;
mod models;
mod store;

pub use error::{LibraryError, Result};
pub use models::{
    content_kind_from_classic, content_kind_to_classic, is_downloadable, is_episode,
    is_podcast_parent, portal_prefs_key, AccountLinkRecord, AccountRecord, AcquireStatus,
    BookRecord, ClaimTicketRecord, EmbeddingRecord, GlobalQueueEntry, ListeningProgressRecord,
    PortalIdentity, RequestStatus, TitleRequestRecord, UserPreferences, WorkRecord,
    OPERATOR_PREFS_KEY,
};
pub use store::{
    fallback_work_key, prefer_enrichment_source, wishlist_identities_match,
    CatalogEnrichmentFields, LibraryStore, NewBook, NewListeningProgress, NewTitleRequest, NewWork,
    SavedFilterRecord, UserBookFields, WishlistIdentity,
};
