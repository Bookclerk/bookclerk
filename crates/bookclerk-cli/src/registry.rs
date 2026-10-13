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
    registry_with_instance(config, outbox, None).await
}

/// [`default_registry_with_plugins`] that selects one plugin instance.
///
/// `instance_id` applies to the plugin key that owns that id. A migrated
/// instance uses its document. Several documents for one key require this id.
///
/// # Errors
///
/// Returns an error when discovery or guest spawn fails, or when instance
/// selection is ambiguous or unknown.
pub async fn registry_with_instance(
    config: &Config,
    outbox: &LibraryStore,
    instance_id: Option<&str>,
) -> anyhow::Result<SourceRegistry> {
    let mut services = SessionServices::from_outbox(Some(outbox));
    services.selected_instance_id = instance_id.map(str::to_string);
    Ok(bookclerk_plugin_host::load_sources(config, &services).await?)
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
    // Do not `?` the alignment result. Its success value is cluster metadata,
    // not this store. A missing or wrong key still fails the open.
    if let Err(err) = bookclerk_library::control_plane::align_cluster_root(
        &store,
        &config.paths().files_dir,
        config.auth_password().as_deref(),
    )
    .await
    {
        return Err(err.into());
    }
    Ok(store)
}

/// Resolve `--source` against registered plugin ids / aliases.
///
/// Two instances that share a key or alias fail closed with the candidate
/// instance ids. They are not reported as an unknown source.
pub fn resolve_source_id(registry: &SourceRegistry, s: &str) -> anyhow::Result<String> {
    match registry.require(s) {
        Ok(source) => Ok(source.id().to_string()),
        Err(err) => {
            let text = err.to_string();
            if text.contains("ambiguous") {
                return Err(anyhow::anyhow!(text));
            }
            let known: Vec<_> = registry
                .all()
                .into_iter()
                .map(|src| src.id().to_string())
                .collect();
            if known.is_empty() {
                Err(anyhow::anyhow!(
                    "unknown source `{s}` (no content sources registered — check `[sources.*] enabled` and plugins/)"
                ))
            } else {
                Err(anyhow::anyhow!(
                    "unknown source `{s}` (registered: {})",
                    known.join(", ")
                ))
            }
        }
    }
}
