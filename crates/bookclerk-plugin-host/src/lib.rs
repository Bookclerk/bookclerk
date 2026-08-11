//! External plugin host for Bookclerk (`bookclerk-plugin-host`).
//!
//! Discovers staged guests under `$BOOKCLERK_FILES_DIR/plugins/`, spawns them
//! over the Workers RPC ABI (native binary or `bookclerk-workerd`), and
//! optionally links first-party adapters in-process when the `bundled-plugins`
//! feature is enabled on the host binary.
//!
//! Two load paths share the same registries:
//!
//! 1. **In-process builtins** — [`register_builtin_sources`] /
//!    [`register_builtin_integrations`] link first-party library crates so
//!    `cargo run` works without staging binaries.
//! 2. **External guests** — separate executables (or `bookclerk-workerd` +
//!    modules) discovered from install directories (`plugin.toml`) over
//!    newline-delimited Workers RPC on stdio.
//!
//! External plugins are **untrusted** relative to the host: the host never
//! passes `library.db` / `master.key` / the files-dir root, clears
//! secret-bearing env on spawn, and mediates credentials + library upserts.
//! Operators must `bookclerk plugins approve` domains/bindings before enable;
//! the same covering grant is required again at every external spawn and at
//! privileged delivery points (`config` / `secrets` / `work_fs` / `oauth`).
//!
//! Host binaries should depend on **this** crate for registration — not on
//! individual store crates.
//!
//! # Guest SDK
//!
//! Third-party Rust plugins should depend on [`bookclerk_plugin_sdk`], not this
//! crate. This host crate re-exports the protocol types for in-tree convenience.
//!
//! See `docs/plugins.md`, `docs/adr/plugin-workers-rpc-workerd.md`, and
//! `docs/plugin-registry.md`.

mod builtins;
mod callback_proxy;
mod consent;
mod crates_io;
mod destinations;
mod discover;
mod error;
#[cfg(unix)]
mod fd_pass;
mod host;
mod jail;
mod manifest;
mod registry;
mod rpc;
#[cfg(windows)]
mod windows_acl;

pub use bookclerk_plugin_sdk::protocol;
pub use bookclerk_plugin_sdk::{
    methods, BookAcquiredDto, CatalogHitDto, CliArgKind, CliArgSpec, CliCommandSpec,
    CliInvokeParams, CliInvokeResult, CliSchema, CredentialsUpdateParams, EventPollResultDto,
    ExpandCandidatesParams, ExternalUserDto, FetchTitleParams, HandshakeResult, HealthDto,
    ListeningProgressDto, LoginCompleteParams, LoginParams, LoginResultDto, LoginStartResultDto,
    PlainPartDto, PluginGuest, PurchaseHintDto, PurchaseHintParams, ScanBookDto, ScanParams,
    ScanSummaryDto, SearchCatalogParams, SourceAccountDto, SourceFetchDto, SyncListeningResultDto,
    PLUGIN_API_VERSION,
};

pub use builtins::{
    load_integrations, load_sources, register_builtin_integrations, register_builtin_sources,
};
pub use consent::{
    consent_request, consent_summary, effective_grant, grant_covers, grant_has_binding,
    handshake_config_for_grant, is_platform_plugin_id, require_binding, require_grant, spawn_grant,
    validate_handshake_capabilities, PluginGrant, PluginGrantStore, GRANTS_FILE,
};
pub use crates_io::search_crates_io;
pub use destinations::{build_acquire_destinations, build_storage_backend};
pub use discover::{discover_plugins, plugin_search_dirs, settings_table, DiscoveredPlugin};
pub use error::{PluginError, Result};
pub use host::{
    load_external_database, load_external_destinations, load_external_integrations,
    load_external_sources, migrate_database_plugin, open_library_store,
    open_library_store_for_plugin, DatabaseRegistry, DestinationRegistry, ExternalDatabase,
    ExternalDestination, ExternalIntegration, ExternalSource,
};
pub use jail::plugin_data_dir;
pub use manifest::{
    embedded_logo_api_path, logo_content_type, validate_logo, BindingCapabilities,
    CapabilitiesManifest, JailNetworkNeed, LogoKind, MethodCapabilities, ModuleSpec,
    NetworkCapabilities, NetworkMode, PluginKind, PluginManifest, PluginRuntimeKind, WorkerdLimits,
    WorkerdRuntimeManifest, MAX_EMBEDDED_LOGO_BYTES,
};
pub use registry::{
    host_target_triple, kind_keyword, validate_plugin_id, BookclerkPackageMetadata,
    PluginCatalogEntry, PluginCrateName, CRATE_NAME_PREFIX, PRODUCT_KEYWORD, REGISTRY_KEYWORD,
};
pub use rpc::PluginClient;

/// Register discovered external plugins into the in-process registries.
pub async fn register_discovered(
    config: &bookclerk_config::Config,
    sources: &mut bookclerk_source::SourceRegistry,
    integrations: &mut bookclerk_integrations::IntegrationRegistry,
) -> Result<()> {
    let plugins = discover_plugins(config)?;
    for plugin in plugins {
        match plugin.manifest.kind {
            PluginKind::Source => {
                if !config.sources.is_enabled(&plugin.manifest.id) {
                    tracing::debug!(
                        id = %plugin.manifest.id,
                        "external source plugin disabled in config; skipping"
                    );
                    continue;
                }
                if sources.get(&plugin.manifest.id).is_some() {
                    tracing::debug!(
                        id = %plugin.manifest.id,
                        path = %plugin.root.join("plugin.toml").display(),
                        "skipping external source — already registered in-process"
                    );
                    continue;
                }
                match ExternalSource::spawn(&plugin, config).await {
                    Ok(source) => {
                        tracing::info!(
                            id = %plugin.manifest.id,
                            path = %plugin.command.display(),
                            "registered external source plugin"
                        );
                        sources.register(std::sync::Arc::new(source));
                    }
                    Err(err) => {
                        tracing::warn!(
                            id = %plugin.manifest.id,
                            %err,
                            "failed to start external source plugin; skipping"
                        );
                    }
                }
            }
            PluginKind::Integration => {
                if !config.integrations.is_enabled(&plugin.manifest.id) {
                    tracing::debug!(
                        id = %plugin.manifest.id,
                        "external integration plugin disabled in config; skipping"
                    );
                    continue;
                }
                if integrations.get(&plugin.manifest.id).is_some() {
                    tracing::debug!(
                        id = %plugin.manifest.id,
                        path = %plugin.root.join("plugin.toml").display(),
                        "skipping external integration — already registered in-process"
                    );
                    continue;
                }
                match ExternalIntegration::spawn(&plugin, config).await {
                    Ok(integration) => {
                        tracing::info!(
                            id = %plugin.manifest.id,
                            path = %plugin.command.display(),
                            "registered external integration plugin"
                        );
                        integrations.register(std::sync::Arc::new(integration));
                    }
                    Err(err) => {
                        tracing::warn!(
                            id = %plugin.manifest.id,
                            %err,
                            "failed to start external integration plugin; skipping"
                        );
                    }
                }
            }
            PluginKind::Output => {
                if !config.output.s3.enabled || plugin.manifest.id != "s3" {
                    tracing::debug!(
                        id = %plugin.manifest.id,
                        "external output plugin skipped (enable [output.s3] for id=s3)"
                    );
                    continue;
                }
                tracing::info!(
                    id = %plugin.manifest.id,
                    "discovered output plugin (loaded via load_external_destinations at startup)"
                );
            }
            PluginKind::Database => {
                if plugin
                    .manifest
                    .id
                    .eq_ignore_ascii_case(&config.database.plugin)
                {
                    tracing::info!(
                        id = %plugin.manifest.id,
                        "discovered database plugin (loaded via load_external_database at startup)"
                    );
                } else {
                    tracing::debug!(
                        id = %plugin.manifest.id,
                        active = %config.database.plugin,
                        "external database plugin skipped (not [database].plugin)"
                    );
                }
            }
        }
    }
    Ok(())
}
