//! Build integration registry from config.

use std::sync::Arc;

use bookclerk_config::Config;
use tracing::info;

use crate::abs::AbsIntegration;
use crate::error::Result;
use crate::registry::IntegrationRegistry;

/// Construct enabled first-party integrations from config.
///
/// When ABS is enabled but misconfigured (e.g. missing API key), it is still
/// registered so health/diagnose/calls surface the error instead of silently
/// omitting the adapter.
pub fn from_config(config: &Config) -> Result<IntegrationRegistry> {
    let mut registry = IntegrationRegistry::new();
    let abs = &config.integrations.audiobookshelf;
    if abs.enabled {
        info!("enabling audiobookshelf integration");
        registry.register(Arc::new(AbsIntegration::from_config(abs.clone())));
    }
    Ok(registry)
}
