//! Dynamic plugins for Bookclerk (host side).
//!
//! Two load paths share the same registries:
//!
//! 1. **In-process builtins** — [`register_builtin_sources`] /
//!    [`register_builtin_integrations`] link first-party library crates so
//!    `cargo run` works without staging binaries.
//! 2. **External guests** — separate executables discovered from install
//!    directories (`plugin.toml` + binary) over newline-delimited JSON-RPC on
//!    stdio.
//!
//! External plugins are **untrusted** relative to the host: the host never
//! passes `library.db` / `master.key` / the files-dir root, clears
//! secret-bearing env on spawn, and mediates credentials + library upserts.
//!
//! Host binaries should depend on **this** crate for registration — not on
//! individual store crates.
//!
//! # Guest SDK
//!
//! Third-party Rust plugins should depend on [`bookclerk_plugin_sdk`], not this
//! crate. This host crate re-exports the protocol types for in-tree convenience.
//!
//! See `docs/plugins.md` and `docs/plugin-registry.md`.

mod builtins;
mod crates_io;
mod discover;
mod error;
#[cfg(unix)]
mod fd_pass;
mod host;
mod jail;
mod manifest;
mod registry;
mod rpc;

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
pub use crates_io::search_crates_io;
pub use discover::{discover_plugins, plugin_search_dirs, settings_table, DiscoveredPlugin};
pub use error::{PluginError, Result};
pub use host::{
    load_external_integrations, load_external_sources, ExternalIntegration, ExternalSource,
};
pub use jail::plugin_data_dir;
pub use manifest::{NetworkNeed, PluginKind, PluginManifest, SandboxManifest};
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
                tracing::warn!(
                    id = %plugin.manifest.id,
                    "output plugins are discovered but not yet loaded (coming soon)"
                );
            }
            PluginKind::Database => {
                tracing::warn!(
                    id = %plugin.manifest.id,
                    "external database plugins are discovered but not yet loaded; \
                     use built-in [database].plugin = \"sqlite\"|\"d1\""
                );
            }
        }
    }
    Ok(())
}
