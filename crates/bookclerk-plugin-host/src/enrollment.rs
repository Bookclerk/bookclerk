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
    current_process_incarnation, default_instance_for_key, ensure_plugin_deployment,
    ensure_plugin_instance, import_instance_config_if_absent, load_deployment_for_host,
    load_plugin_placement, pin_plugin_schema, reconcile_instance_configs_to_schema,
    upsert_observation, ConfigActor, DeploymentObservation, DeploymentStatus, PinnedPluginSchema,
    PluginDeployment, PluginInstance, PluginInstanceConfigV1, SchemaPin, SchemaPinChange,
    DESIRED_ABSENT, DESIRED_PRESENT,
};
use bookclerk_library::{LibraryError, LibraryStore};

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
    let package_version = plugin
        .manifest
        .package_version()
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    let schema_body = plugin.manifest.settings_schema();
    let pinned = pin_plugin_schema(
        store,
        &SchemaPin {
            plugin_key: &key,
            manifest_id: &plugin.manifest.id,
            package_version: &package_version,
            schema: &schema_body,
            host_id,
        },
    )
    .await
    .map_err(library_err)?;
    match &pinned.change {
        SchemaPinChange::Stale { local_version } => {
            let detail = format!(
                "installed version {local_version} is older than {} pinned by host {}; not loaded",
                pinned.schema.package_version, pinned.schema.recorded_by_host
            );
            record_stale_version(store, host_id, &key, &detail).await;
            return Err(crate::PluginError::message(format!(
                "plugin `{key}`: {detail}"
            )));
        }
        SchemaPinChange::Upgraded { previous_version } => tracing::info!(
            plugin_key = %key,
            from = %previous_version,
            to = %package_version,
            "plugin schema pin upgraded"
        ),
        SchemaPinChange::Repinned => tracing::warn!(
            plugin_key = %key,
            version = %package_version,
            "plugin schema changed without a version bump; re-pinned"
        ),
        SchemaPinChange::RolledBack { previous_version } => tracing::info!(
            plugin_key = %key,
            from = %previous_version,
            to = %package_version,
            "plugin schema pin rolled back with the active release"
        ),
        SchemaPinChange::Pinned | SchemaPinChange::Unchanged => {}
    }
    // Every pass reconciles the documents against the pinned schema, not
    // only the pass that moved the pin: a write that lost a race, or a value
    // whose type changed, is picked up on the next restart.
    let reconciled = reconcile_instance_configs_to_schema(store, &key, &pinned.schema.schema)
        .await
        .map_err(library_err)?;
    for change in &reconciled {
        tracing::warn!(
            plugin_key = %key,
            plugin_instance_id = %change.instance_id,
            removed = ?change.removed_keys,
            revision = change.revision,
            "instance document keys the pinned schema no longer accepts were dropped"
        );
    }
    let schema = pinned.schema;
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
            // A scheduler-placed instance is the scheduler's to deploy:
            // enrollment records this host as `absent` and never `present`.
            let placed = load_plugin_placement(store, &instance.id)
                .await
                .map_err(library_err)?
                .is_some();
            let desired = if !placed && has_covering_grant(plugin, grants) {
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

/// Writes an `error` observation on this host's deployment of the default
/// instance so the stale package is visible in Settings, when such a
/// deployment exists. Nothing else is written.
async fn record_stale_version(store: &LibraryStore, host_id: &str, plugin_key: &str, detail: &str) {
    let instance = match default_instance_for_key(store, plugin_key).await {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(plugin_key, error = %err, "stale-version observation: instance read failed");
            return;
        }
    };
    let deployment = match load_deployment_for_host(store, &instance.id, host_id).await {
        Ok(Some(deployment)) => deployment,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(plugin_key, error = %err, "stale-version observation: deployment read failed");
            return;
        }
    };
    let observation = DeploymentObservation {
        deployment_id: deployment.deployment_id,
        host_id: host_id.to_string(),
        incarnation: current_process_incarnation().to_string(),
        status: DeploymentStatus::Error,
        detail: detail.to_string(),
        applied_config_revision: None,
        observed_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(err) = upsert_observation(store, &observation).await {
        tracing::warn!(plugin_key, error = %err, "stale-version observation write failed");
    }
}

/// Maps a library error onto the host error type.
fn library_err(err: LibraryError) -> crate::PluginError {
    crate::PluginError::message(err.to_string())
}
