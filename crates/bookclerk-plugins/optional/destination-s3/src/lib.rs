//! S3 / MinIO destination plugin shared constants.
//!
//! Optional destination that writes acquire output to a bucket configured via
//! `[output.s3]` / `BOOKCLERK_OUTPUT_S3_*`. See [`guest`] for v1 JSON helpers
//! and [`v2`] for the product Cap'n Proto destination / job-handler surface.

pub mod guest;
pub mod v2;

/// Plugin id string advertised in handshake and config (`[output.s3]`).
pub const ID: &str = "s3";
