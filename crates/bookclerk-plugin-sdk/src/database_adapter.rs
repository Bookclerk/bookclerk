//! Database adapter helpers for third-party database plugin authors.
//!
//! # Audience
//!
//! Rust authors building a **database** guest (`kind = "database"`). Use this
//! module with the `db` feature enabled on `bookclerk-plugin-sdk`:
//!
//! ```toml
//! [dependencies]
//! bookclerk-plugin-sdk = { version = "0.1", features = ["db"] }
//! ```
//!
//! # What to use
//!
//! | Need | Entry point |
//! | --- | --- |
//! | Engine error → structured [`crate::PluginError`] | [`plugin_error_from_engine`], [`plugin_error_from_db_err`] |
//! | Host-provided SQL scripts (no Bookclerk migrations) | [`execute_sql_scripts`] |
//! | Typed SQL `NULL` for proxy row decode | [`typed_null`] |
//!
//! First-party platform database guests use the internal `bookclerk-db-guest`
//! crate for SeaORM session workers; those helpers are not part of the stable
//! third-party author API.

pub mod errors;
pub mod migrate;

pub use errors::{plugin_error_from_db_err, plugin_error_from_engine};
pub use migrate::{execute_sql_scripts, split_sql_statements, typed_null};
