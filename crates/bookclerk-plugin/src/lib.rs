//! Dynamic third-party plugins for Bookclerk.
//!
//! Plugins are **separate executables** discovered from install directories
//! (`plugin.toml` + binary) and spoken to over newline-delimited JSON-RPC on
//! stdio. User settings stay in the main `config.toml` under matching
//! `[sources.<id>]` / `[integrations.<id>]` tables and are passed at handshake.
//!
//! # Layout
//!
//! ```text
//! $BOOKCLERK_FILES_DIR/plugins/
//!   my-plugin/
//!     plugin.toml          # install metadata (id, kind, command)
//!     my-plugin            # executable
//! ```
//!
//! Additional search roots: `BOOKCLERK_PLUGIN_DIRS` (path-list, OS-separated).
//!
//! ```toml
//! # config.toml — enable + opaque knobs only
//! [integrations.echo]
//! enabled = true
//! # greeting = "hi"
//! ```
//!
//! See `docs/plugins.md`.

mod discover;
mod error;
mod host;
mod manifest;
pub mod protocol;
mod rpc;

pub use discover::{discover_plugins, plugin_search_dirs, settings_table, DiscoveredPlugin};
pub use error::{PluginError, Result};
pub use host::{
    load_external_integrations, load_external_sources, ExternalIntegration, ExternalSource,
};
pub use manifest::{PluginKind, PluginManifest};
pub use protocol::{
    methods, CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    HandshakeResult, HealthDto, PLUGIN_API_VERSION,
};
pub use rpc::{PluginClient, PluginGuest};

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
                    return Err(PluginError::message(format!(
                        "external source plugin id `{}` conflicts with an already registered source ({})",
                        plugin.manifest.id,
                        plugin.root.join("plugin.toml").display()
                    )));
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
                    return Err(PluginError::message(format!(
                        "external integration plugin id `{}` conflicts with an already registered integration ({})",
                        plugin.manifest.id,
                        plugin.root.join("plugin.toml").display()
                    )));
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
        }
    }
    Ok(())
}
