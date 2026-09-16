//! Optional Cloudflare D1 library database plugin.
//!
//! Alternate `[database]` backend for remote D1. Hosts call [`open`] after
//! operator config supplies account/database credentials; see `docs/database.md`.
//! Atomic library writes use [`D1Proxy::run_typed_atomic`] rather than
//! interactive `BEGIN`.

pub mod atomic;
pub mod d1;

pub use d1::{
    delete_database, ensure_database, lookup_database, open, set_shared_proxy, shared_proxy,
    D1Proxy,
};
