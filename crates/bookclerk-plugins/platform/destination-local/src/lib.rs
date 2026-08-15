//! Local filesystem destination plugin shared constants.
//!
//! The platform `local` guest writes acquire output under
//! `[output.local]` / `BOOKCLERK_OUTPUT_LOCAL_ROOT`. See
//! [`guest`] for the v1 JSON helpers and [`v2`] for the product Cap'n Proto
//! destination / job-handler surface.

pub mod guest;
pub mod v2;

/// Plugin id string advertised in handshake and config (`[output.local]`).
pub const ID: &str = "local";
