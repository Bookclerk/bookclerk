//! Shared helpers for first-party database plugin guests.
//!
//! Engine-specific connect/proxy code lives in each guest crate; this crate
//! owns the per-process SeaORM session (ping/query/execute) and greenfield
//! migration helpers used by D1 / Postgres.

pub mod migrate;
pub mod session;

pub use migrate::apply_pending_migrations;
pub use session::{guest_execute, guest_ping, guest_query, row_to_dto, set_connection};
