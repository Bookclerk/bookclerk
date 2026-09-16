//! Local filesystem destination plugin shared constants.
//!
//! The platform `local` guest writes acquire output under
//! `[output.local]` / `BOOKCLERK_OUTPUT_LOCAL_ROOT`. See
//! [`guest`] for the v1 JSON helpers and [`plugin`] for the product Cap'n Proto
//! destination / job-handler surface.

pub mod guest;
pub mod plugin;

/// Plugin id string advertised in describe() and config (`[output.local]`).
pub const ID: &str = "local";
