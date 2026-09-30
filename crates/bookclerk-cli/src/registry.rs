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

/// Integrations via the plugin host (in-process builtins + externals);
/// `outbox` backs the guests' `EVENTS` binding.
pub async fn integrations_with_plugins(
    config: &Config,
    outbox: &LibraryStore,
) -> anyhow::Result<bookclerk_integrations::IntegrationRegistry> {
    Ok(bookclerk_plugin_host::load_integrations(
        config,
        &SessionServices::from_outbox(Some(outbox)),
    )
    .await?)
}

/// Open the library and align the cluster secret root before returning it.
///
/// Alignment runs before the caller can cache a data-encryption key or seal a
/// secret. An enrolled database with a missing or different `master.key` fails
/// here, and this function does not leave a replacement key behind.
pub async fn open_library(config: &Config) -> anyhow::Result<LibraryStore> {
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let store = bookclerk_plugin_host::open_library_store(config, &registry).await?;
    bookclerk_library::control_plane::align_cluster_root(
        &store,
        &config.paths().files_dir,
        config.auth_password().as_deref(),
    )
    .await?;
    Ok(store)
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
