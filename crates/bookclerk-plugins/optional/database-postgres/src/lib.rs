//! Optional PostgreSQL library database plugin.
//!
//! Alternate `[database]` backend for a networked Postgres instance. Hosts call
//! [`open`] with the configured connection URL; see `docs/database.md`.

pub mod postgres;
mod socket_mediate;

pub use postgres::{
    drop_binding, open, open_binding, open_binding_existing, postgres_tcp_target,
    MIN_POSTGRES_MAJOR, MIN_POSTGRES_VERSION_NUM,
};
