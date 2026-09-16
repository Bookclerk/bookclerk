//! S3 / MinIO destination plugin shared constants.
//!
//! Optional destination that writes acquire output to a bucket configured via
//! `[output.s3]` / `BOOKCLERK_OUTPUT_S3_*`. See [`plugin`] for the Cap'n Proto
//! destination / job-handler surface.

pub mod plugin;

/// Plugin id string advertised in describe() and config (`[output.s3]`).
pub const ID: &str = "s3";
