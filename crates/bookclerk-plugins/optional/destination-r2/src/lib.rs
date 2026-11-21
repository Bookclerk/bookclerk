//! Cloudflare R2 destination plugin.
//!
//! Writes acquire output to an R2 bucket through R2's S3 API, which
//! enforces conditional writes and serves batch deletes and server-side
//! copies, and follows the bucket's changes through R2 event notifications
//! pulled from a Cloudflare Queue. See [`plugin`] for the Cap'n Proto
//! destination / job-handler surface.

pub mod plugin;

/// Plugin id advertised in describe() and the manifest.
pub const ID: &str = "r2";
