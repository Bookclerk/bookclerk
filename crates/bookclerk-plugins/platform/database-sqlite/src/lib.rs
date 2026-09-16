//! Platform SQLite library database plugin.
//!
//! Default `[database]` backend for Bookclerk: opens `library.db` under
//! `$BOOKCLERK_FILES_DIR` and exposes a SeaORM proxy. The host applies schema
//! after connect. Prefer [`open_store`] from tests; guests speak the DB ABI
//! through `bookclerk-plugin-sdk`.

/// Plugin id advertised in describe and `plugin.toml`.
pub const ID: &str = "sqlite";

pub mod plugin;
pub mod sqlite;

pub use sqlite::{open, open_memory_unmigrated};
#[cfg(feature = "host-helpers")]
pub use sqlite::{open_memory, open_store, open_store_memory};
