use bookclerk_config::Config;
use bookclerk_source::SourceRegistry;

/// Content sources via the plugin host (in-process builtins + externals).
///
/// Host binaries do not name store crates — [`bookclerk_plugin::load_sources`]
/// registers first-party adapters in-process and loads discovered guests.
pub async fn default_registry_with_plugins(config: &Config) -> anyhow::Result<SourceRegistry> {
    Ok(bookclerk_plugin::load_sources(config).await?)
}
