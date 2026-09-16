use bookclerk_config::Config;
use bookclerk_library::LibraryStore;
use bookclerk_plugin_host::SessionServices;
use bookclerk_source::SourceRegistry;

/// Content sources via the plugin host (in-process builtins + externals).
///
/// Host binaries do not name store crates — [`bookclerk_plugin_host::load_sources`]
/// registers first-party adapters in-process and loads discovered guests.
/// `outbox` backs the guests' `EVENTS` binding.
pub async fn default_registry_with_plugins(
    config: &Config,
    outbox: &LibraryStore,
) -> anyhow::Result<SourceRegistry> {
    Ok(
        bookclerk_plugin_host::load_sources(config, &SessionServices::from_outbox(Some(outbox)))
            .await?,
    )
}
