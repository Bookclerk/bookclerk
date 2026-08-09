//! First-party library database plugin (SQLite, D1, Postgres).
//!
//! Engine-specific connect / proxy / migrate code lives in [`sqlite`], [`d1`],
//! and [`postgres`]. The JSON-RPC guest is [`guest`]. Hosts must talk to this
//! process over RPC; tests and CLI helpers may call [`sqlite::open_memory`]
//! directly.

pub mod d1;
pub mod guest;
pub mod migrate;
pub mod postgres;
pub mod sqlite;
