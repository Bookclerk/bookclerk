//! Optional PostgreSQL library database plugin.
//!
//! Alternate `[database]` backend for a networked Postgres instance. Hosts call
//! [`open`] with the configured connection URL; see `docs/database.md`.

pub mod postgres;

pub use postgres::open;
