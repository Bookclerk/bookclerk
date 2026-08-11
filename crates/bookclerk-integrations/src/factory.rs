//! Build integration registry from config.
//!
//! First-party adapters register through
//! `bookclerk_plugin_host::register_builtin_integrations` (feature-gated plugin
//! crates). This module keeps [`from_config`] / [`register_builtins`] as
//! stable no-ops so older call sites still compile.

use bookclerk_config::Config;

use crate::error::Result;
use crate::registry::IntegrationRegistry;

/// Register first-party integrations into an existing registry.
///
/// Prefer `bookclerk_plugin_host::register_builtin_integrations` from hosts.
/// This function is intentionally a no-op; ABS and other adapters register
/// from their plugin packages.
///
/// # Arguments
///
/// * `config` - Loaded Bookclerk configuration.
/// * `registry` - Configured content-source or integration registry.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn register_builtins(_config: &Config, _registry: &mut IntegrationRegistry) -> Result<()> {
    Ok(())
}

/// Construct an empty registry (no in-process adapters).
///
/// Hosts that also load plugins should prefer
/// `bookclerk_plugin_host::load_integrations`.
///
/// # Arguments
///
/// * `config` - Loaded Bookclerk configuration.
///
/// # Returns
///
/// On success, the inner `IntegrationRegistry` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn from_config(_config: &Config) -> Result<IntegrationRegistry> {
    Ok(IntegrationRegistry::new())
}
