//! In-process registration of first-party plugins.
//!
//! Hosts (`bookclerk` / `bookclerkd`) call these helpers instead of naming
//! store crates. That keeps binaries store-agnostic while still allowing
//! `cargo run` without staging external plugin binaries — first-party plugin
//! packages under `crates/bookclerk-plugins/` expose a library (`register`)
//! linked through this host crate when the matching Cargo feature is enabled.
//!
//! Feature names match plugin package names (`bookclerk-plugin-source-audible`,
//! `bookclerk-plugin-integration-audiobookshelf`, …). Build with
//! `--no-default-features` for external-guest-only hosts.
//! [`crate::load_external_sources`] / [`crate::load_external_integrations`]
//! always run and skip ids already registered here.

use bookclerk_config::Config;
use bookclerk_integrations::IntegrationRegistry;
use bookclerk_source::SourceRegistry;

/// Register first-party content sources in-process when their Cargo features
/// are enabled and the source is enabled in config.
pub fn register_builtin_sources(config: &Config, registry: &mut SourceRegistry) {
    #[cfg(feature = "bookclerk-plugin-source-audible")]
    bookclerk_plugin_source_audible::register(registry, config);
    #[cfg(feature = "bookclerk-plugin-source-libro")]
    bookclerk_plugin_source_libro::register(registry, config);
    #[cfg(feature = "bookclerk-plugin-source-graphicaudio")]
    bookclerk_plugin_source_graphicaudio::register(registry, config);
    #[cfg(feature = "bookclerk-plugin-source-chirp")]
    bookclerk_plugin_source_chirp::register(registry, config);
    let _ = (config, registry);
}

/// Register first-party integrations in-process when their Cargo features are
/// enabled and the integration is enabled in config.
///
/// Misconfigured-but-enabled ABS is still registered so health/diagnose surface
/// the error (same behavior as the former in-crate factory).
pub fn register_builtin_integrations(
    config: &Config,
    registry: &mut IntegrationRegistry,
) -> crate::Result<()> {
    #[cfg(feature = "bookclerk-plugin-integration-audiobookshelf")]
    {
        bookclerk_plugin_integration_audiobookshelf::register(registry, config)
            .map_err(|e| crate::PluginError::message(e.to_string()))?;
    }
    let _ = (config, registry);
    Ok(())
}

/// Built-in sources plus discovered external source plugins.
pub async fn load_sources(config: &Config) -> crate::Result<SourceRegistry> {
    let mut registry = SourceRegistry::new();
    register_builtin_sources(config, &mut registry);
    crate::load_external_sources(config, &mut registry).await?;
    Ok(registry)
}

/// Built-in integrations plus discovered external integration plugins.
pub async fn load_integrations(config: &Config) -> crate::Result<IntegrationRegistry> {
    let mut registry = IntegrationRegistry::new();
    register_builtin_integrations(config, &mut registry)?;
    crate::load_external_integrations(config, &mut registry).await?;
    Ok(registry)
}
