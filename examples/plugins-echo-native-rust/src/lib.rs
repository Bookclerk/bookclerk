//! Reference Echo integration (native Rust): the parts its tests drive.
//!
//! [`notes`] keeps a `notes` table on the plugin's own `[[databases]]`
//! binding, written against the database limits; [`cli`] is the `cli`
//! entrypoint that exposes it. `src/main.rs` serves them with the other
//! entrypoints.

pub mod cli;
pub mod notes;
