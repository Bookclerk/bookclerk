//! Optional Cloudflare D1 library database plugin.
//!
//! Alternate `[database]` backend for remote D1. Hosts call [`open`] after
//! operator config supplies account/database credentials; see `docs/database.md`.

pub mod d1;

pub use d1::open;
