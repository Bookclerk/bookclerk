//! S3 / MinIO destination plugin shared constants.
//!
//! Optional storefront destination that writes acquire output to a bucket
//! configured via `[output.s3]` / `BOOKCLERK_OUTPUT_S3_*`. See [`guest`] for
//! the Workers RPC storage helpers the host invokes.

pub mod guest;

/// Plugin id string advertised in handshake and config (`[output.s3]`).
pub const ID: &str = "s3";
