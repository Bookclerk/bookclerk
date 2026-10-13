//! Generic plugin enrollment for one host.
//!
//! Every plugin discovered on this host, except database adapters (which
//! stay bootstrap until Phase 4), gets the same treatment regardless of
//! family, alias, or provenance:
//!
//! 1. its `[[settings]]` schema is pinned in `plugin_schemas` so any API host
//!    can validate a document for it;
//! 2. its default [`PluginInstance`] exists with an (initially empty)
//!    configuration document — values come from schema defaults until an
//!    operator writes some;
//! 3. a [`PluginDeployment`] for this host exists. It is created `present`
//!    when the plugin already has a covering grant (platform guests grant
//!    themselves; an operator approved everything else), otherwise `absent`
//!    until `bookclerk plugins enable` sets it.
//!
//! Enrollment is idempotent and runs in both the CLI and the daemon, so the
//! two see the same instances and deployments.

use bookclerk_library::control_plane::{
    ensure_plugin_deployment, ensure_plugin_instance, import_instance_config_if_absent,
    load_deployment_for_host, pin_plugin_schema, ConfigActor, PinnedPluginSchema, PluginDeployment,
    PluginInstance, PluginInstanceConfigV1, SchemaPin, DESIRED_ABSENT, DESIRED_PRESENT,
};
use bookclerk_library::{LibraryError, LibraryStore};
use sha2::{Digest, Sha256};

use crate::consent::PluginGrantStore;
use crate::discover::DiscoveredPlugin;
use crate::Result;

/// One plugin after enrollment on this host.
#[derive(Debug, Clone)]
pub struct EnrolledPlugin {
    /// Discovered install.
    pub plugin: DiscoveredPlugin,
    /// Default instance for the plugin key.
    pub instance: PluginInstance,
    /// This host's deployment of the default instance.
    pub deployment: PluginDeployment,
    /// Pinned settings schema.
    pub schema: PinnedPluginSchema,
}

/// True when the plugin is a database adapter and nothing else: those stay
/// bootstrap (`[database]` in `config.toml`) in this phase.
#[must_use]
pub fn is_bootstrap_only(plugin: &DiscoveredPlugin) -> bool {
    plugin.manifest.families() == vec![crate::PluginFamily::Database]
}

/// Enrolls every discovered plugin on `host_id`.
///
/// A plugin that fails to enroll is logged and skipped so one bad install
/// does not keep the others from starting.
///
/// # Errors
///
/// Returns an error when discovery or the grant store cannot be read.
pub async fn enroll_local_plugins(
    store: &LibraryStore,
    config: &bookclerk_config::Config,
    host_id: &str,
) -> Result<Vec<EnrolledPlugin>> {
    let plugins = crate::discover_plugins(config)?;
    let grants = PluginGrantStore::load(&config.paths().files_dir)?;
    let mut enrolled = Vec::new();
    for plugin in plugins {
        if is_bootstrap_only(&plugin) {
            continue;
        }
        if plugin.manifest.api_version != bookclerk_plugin_sdk::PRODUCT_API_VERSION {
            tracing::warn!(
                id = %plugin.manifest.id,
                api_version = plugin.manifest.api_version,
                "plugin is not api_version 3; not enrolled"
            );
            continue;
        }
        match enroll_one(store, host_id, &plugin, &grants).await {
            Ok(one) => enrolled.push(one),
            Err(err) => tracing::warn!(
                id = %plugin.manifest.id,
                plugin_key = %plugin.plugin_key().canonical(),
                error = %err,
                "plugin enrollment failed; skipping"
            ),
        }
    }
    Ok(enrolled)
}

/// Enrolls one discovered plugin. See the module docs for the steps.
///
/// # Errors
///
/// Returns an error when a schema, instance, document, or deployment write fails.
pub async fn enroll_one(
    store: &LibraryStore,
    host_id: &str,
    plugin: &DiscoveredPlugin,
    grants: &PluginGrantStore,
) -> Result<EnrolledPlugin> {
    let key = plugin.plugin_key().canonical().to_string();
    let schema_body = plugin.manifest.settings_schema();
    let manifest_sha256 = manifest_sha256(plugin)?;
    let schema = pin_plugin_schema(
        store,
        &SchemaPin {
            plugin_key: &key,
            manifest_id: &plugin.manifest.id,
            manifest_sha256: &manifest_sha256,
            package_version: plugin.manifest.version.as_deref().unwrap_or(""),
            schema: &schema_body,
            host_id,
        },
    )
    .await
    .map_err(library_err)?;
    let actor = ConfigActor::Bootstrap;
    let instance = ensure_plugin_instance(store, &actor, &key)
        .await
        .map_err(library_err)?;
    import_instance_config_if_absent(
        store,
        &actor,
        &instance.id,
        &schema_body,
        &PluginInstanceConfigV1::empty(),
        &format!("seed-instance-config-{}", instance.id),
    )
    .await
    .map_err(library_err)?;
    let deployment = match load_deployment_for_host(store, &instance.id, host_id)
        .await
        .map_err(library_err)?
    {
        Some(existing) => existing,
        None => {
            let desired = if has_covering_grant(plugin, grants) {
                DESIRED_PRESENT
            } else {
                DESIRED_ABSENT
            };
            ensure_plugin_deployment(store, &actor, &instance.id, host_id, desired)
                .await
                .map_err(library_err)?
        }
    };
    Ok(EnrolledPlugin {
        plugin: plugin.clone(),
        instance,
        deployment,
        schema,
    })
}

/// True when the operator already consented to this plugin, or it is a
/// verified platform guest that grants itself on first spawn.
#[must_use]
pub fn has_covering_grant(plugin: &DiscoveredPlugin, grants: &PluginGrantStore) -> bool {
    if grants
        .get_by_plugin_key(plugin.plugin_key().canonical())
        .is_some()
    {
        return true;
    }
    plugin.identity.provenance.grants_platform_defaults()
        && bookclerk_plugin_catalog::is_platform_plugin_key(plugin.plugin_key())
}

/// SHA-256 of the installed `plugin.toml`, hex encoded.
fn manifest_sha256(plugin: &DiscoveredPlugin) -> Result<String> {
    let text = std::fs::read(plugin.root.join("plugin.toml")).map_err(|err| {
        crate::PluginError::message(format!(
            "read {}: {err}",
            plugin.root.join("plugin.toml").display()
        ))
    })?;
    Ok(hex::encode(Sha256::digest(text)))
}

fn library_err(err: LibraryError) -> crate::PluginError {
    crate::PluginError::message(err.to_string())
}
