//! Backblaze B2 destination plugin.
//!
//! Writes acquire output to a B2 bucket through B2's native API, with
//! large files for long uploads, server-side copies and probes from the
//! download URL. B2 has no conditional writes, so the host checks before
//! each conditional write and reports that it was not atomic. See
//! [`plugin`] for the Cap'n Proto destination / job-handler surface.

pub mod plugin;

/// Plugin id advertised in describe() and the manifest.
pub const ID: &str = "b2";
