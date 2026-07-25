//! Canonical Libation library database (SQLite via rusqlite).

mod error;
mod migrations;
mod models;
mod store;

pub use error::{LibraryError, Result};
pub use models::{
    content_kind_from_classic, content_kind_to_classic, is_downloadable, is_episode,
    is_podcast_parent, AccountLinkRecord, AccountRecord, BookRecord, ClaimTicketRecord,
    LiberateStatus, PortalIdentity,
};
pub use store::{
    prefer_enrichment_source, LibraryStore, NewBook, SavedFilterRecord, UserBookFields,
};
