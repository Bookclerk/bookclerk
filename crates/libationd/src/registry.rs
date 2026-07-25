//! Default content-source registry (first-party + discovered plugins).

use libation_config::Config;
use libation_source::SourceRegistry;

/// Build a registry with enabled first-party content sources from config.
#[must_use]
pub fn default_registry(config: &Config) -> SourceRegistry {
    let mut registry = SourceRegistry::new();
    libation_audible::register(&mut registry, config);
    libation_libro::register(&mut registry, config);
    libation_graphicaudio::register(&mut registry, config);
    libation_chirp::register(&mut registry, config);
    registry
}

/// First-party sources plus dynamically discovered external plugins.
///
/// Fails hard on plugin id conflicts so the daemon does not run with an
/// ambiguous source set.
pub async fn default_registry_with_plugins(config: &Config) -> anyhow::Result<SourceRegistry> {
    let mut registry = default_registry(config);
    libation_plugin::load_external_sources(config, &mut registry).await?;
    Ok(registry)
}
