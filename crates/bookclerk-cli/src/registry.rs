use bookclerk_config::Config;
use bookclerk_source::SourceRegistry;

/// Content sources via the plugin host (in-process builtins + externals).
///
/// Host binaries do not name store crates — [`bookclerk_plugin::load_sources`]
/// registers first-party adapters in-process and loads discovered guests.
pub async fn default_registry_with_plugins(config: &Config) -> anyhow::Result<SourceRegistry> {
    Ok(bookclerk_plugin::load_sources(config).await?)
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
                "unknown source `{s}` (no content sources registered — check `[sources.*] enabled` and plugins/)"
            )
        } else {
            anyhow::anyhow!("unknown source `{s}` (registered: {})", known.join(", "))
        }
    })
}
