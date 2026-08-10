//! Platform SQLite library database plugin.

pub mod sqlite;

pub use sqlite::{open, open_memory, open_store, open_store_memory};
