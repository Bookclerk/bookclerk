//! Platform SQLite library database plugin.
//!
//! Default `[database]` backend for Bookclerk: opens `library.db` under
//! `$BOOKCLERK_FILES_DIR`, runs SeaORM migrations, and exposes a
//! [`LibraryStore`](bookclerk_library::LibraryStore). Prefer
//! [`open_store`] from hosts; guests speak the DB ABI
//! through `bookclerk-plugin-sdk`.

/// Plugin id advertised in describe and `plugin.toml`.
pub const ID: &str = "sqlite";

pub mod sqlite;
pub mod v2;

pub use sqlite::{open, open_memory, open_store, open_store_memory};
