//! Default content-source registry (first-party + discovered plugins).

use bookclerk_config::Config;
use bookclerk_source::SourceRegistry;

/// Build a registry with enabled first-party content sources from config.
#[must_use]
pub fn default_registry(config: &Config) -> SourceRegistry {
    let mut registry = SourceRegistry::new();
    bookclerk_audible::register(&mut registry, config);
    bookclerk_libro::register(&mut registry, config);
    bookclerk_graphicaudio::register(&mut registry, config);
    bookclerk_chirp::register(&mut registry, config);
    registry
}

/// First-party sources plus dynamically discovered external plugins.
///
/// Fails hard on plugin id conflicts (duplicate discovered manifests or clash
/// with a first-party source) so the process does not continue ambiguously.
pub async fn default_registry_with_plugins(config: &Config) -> anyhow::Result<SourceRegistry> {
    let mut registry = default_registry(config);
    bookclerk_plugin::load_external_sources(config, &mut registry).await?;
    Ok(registry)
}

/// Resolve `--source` against registered plugin ids / aliases.
pub fn resolve_source_id(registry: &SourceRegistry, s: &str) -> anyhow::Result<String> {
    registry.resolve_id(s).ok_or_else(|| {
        let known: Vec<_> = registry
            .all()
            .into_iter()
            .map(|src| src.id().to_string())
            .collect();
        if known.is_empty() {
            anyhow::anyhow!(
                "unknown source `{s}` (no content sources registered — check `[sources.*] enabled`)"
            )
        } else {
            anyhow::anyhow!("unknown source `{s}` (registered: {})", known.join(", "))
        }
    })
}
