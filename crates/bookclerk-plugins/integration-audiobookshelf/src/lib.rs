//! Audiobookshelf integration plugin: in-process [`Integration`] + JSON-RPC guest.
//!
//! Host binaries should register via [`register`] through
//! `bookclerk_plugin_host::register_builtin_integrations` (feature-gated), not by
//! depending on this crate directly.

mod brand;
mod client;
pub mod guest;
mod integration;
mod listening;

pub use brand::{brand_for_id, matches_id, BRAND};
pub use client::{
    AbsApiClient, AbsLibrary, AbsLibraryItem, AbsMediaProgress, AbsUser, AbsUserDetail,
};
pub use guest::AbsGuestState;
pub use integration::AbsIntegration;
pub use listening::{collect_listening_snapshots, sync_listening_progress};

use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_integrations::{IntegrationRegistry, Result};
use tracing::info;

/// Integration id (`audiobookshelf`).
pub const ID: &str = "audiobookshelf";

/// Register Audiobookshelf when `[integrations.audiobookshelf] enabled`.
///
/// When enabled but misconfigured (e.g. missing API key), the adapter is still
/// registered so health/diagnose surface the error instead of silently omitting
/// it.
pub fn register(registry: &mut IntegrationRegistry, config: &Config) -> Result<()> {
    let abs = config.integrations.audiobookshelf();
    if abs.enabled {
        info!("enabling audiobookshelf integration");
        registry.register(Arc::new(AbsIntegration::from_config(abs)));
    }
    Ok(())
}
