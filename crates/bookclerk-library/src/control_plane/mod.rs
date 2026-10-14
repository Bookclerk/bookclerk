//! Database-backed control plane: host identity and typed configuration.
//!
//! Bootstrap material (database target, `master.key`, `host-identity.json`) is
//! read before these rows exist. After enrollment, `core.events` is the
//! authority for `[events]`. Other `config.toml` sections stay transitional.
//!
//! See `docs/adr/control-plane.md`.

mod batch;
mod deployments;
mod documents;
mod host_queue;
mod host_status;
mod identity;
mod instance_config;
mod instances;
mod placement;
mod releases;
mod schemas;
mod secret;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use deployments::{
    bounded_observation_detail, delete_plugin_deployment, ensure_plugin_deployment,
    instance_has_deployment, list_all_deployments, list_deployments_for_host,
    list_deployments_for_instance, list_observations_for_deployment,
    list_present_deployments_for_host, load_deployment, load_deployment_for_host, load_observation,
    present_plugin_keys_for_host, replace_plugin_deployment, upsert_observation,
    DeploymentObservation, DeploymentReplace, DeploymentStatus, DesiredState, PluginDeployment,
    MAX_OBSERVATION_DETAIL_BYTES,
};
pub use documents::{
    audit_count, change_count, ConfigActor, DocumentKey, ReplaceOutcome, StoredDocument,
    CLUSTER_SCOPE_ID, CLUSTER_SCOPE_TYPE, HOST_SCOPE_TYPE, MAX_CONFIGURATION_DOCUMENT_BYTES,
    PLUGIN_INSTANCE_CONFIG_NAMESPACE, PLUGIN_INSTANCE_SCOPE_TYPE,
};
pub use host_queue::{
    host_consumer_id, host_queue_name, is_host_consumer_id, publish_host_message,
    publish_to_all_hosts, HostMessage, HOST_CONSUMER_PREFIX,
};
pub use host_status::{
    host_reservations, instance_demand, list_host_statuses, load_host_status, next_availability,
    record_deployment_reservation, record_host_heartbeat, GuestReservation, HostHeartbeat,
    HostSampler, HostStatus, AVAILABILITY_FULL_PPM, AVAILABILITY_TAU, DEFAULT_GUEST_CPU_MILLICORES,
    DEFAULT_GUEST_MEMORY_BYTES, HOST_HEARTBEAT_INTERVAL, HOST_LIVE_INTERVALS,
};
pub use identity::{
    current_process_incarnation, host_identity_path, host_target, list_host_ids, list_hosts,
    load_host, load_or_create_host_identity, HostIdentity, HostRecord, HOST_IDENTITY_FILE,
};
pub use instance_config::{
    import_instance_config_if_absent, load_instance_config, parse_setting_text,
    reconcile_instance_configs_to_schema, replace_instance_config, resolve_instance_bindings,
    seal_instance_secret, set_secret_ref, set_setting, InstanceBindingGrant,
    InstanceConfigDocument, InstanceConfigReplace, InstanceSecretRefV1, PluginInstanceConfigV1,
    PrunedInstanceConfig, ResolvedInstanceBindings, SettingValue, MAX_INSTANCE_SECRET_REFS,
    MAX_INSTANCE_SETTINGS, MAX_INSTANCE_SETTING_BYTES, PLUGIN_INSTANCE_CONFIG_SCHEMA_VERSION,
};
pub use instances::{
    create_plugin_instance, default_instance_for_key, delete_plugin_instance,
    ensure_plugin_instance, find_plugin_instance_by_key, list_all_plugin_instances,
    list_plugin_instances_for_key, load_plugin_instance, PluginInstance, PluginInstanceId,
};
pub use placement::{
    acquire_lease, delete_plugin_placement, effective_host_labels, ensure_hand_managed,
    labels_satisfy, list_plugin_placements, live_host_ids, load_lease, load_plugin_placement,
    plan_placement, put_plugin_placement, release_lease, schedule_placements, set_host_labels,
    validate_constraint, validate_host_label, validate_label, ClusterLease, HostCandidate,
    PlacementPlan, PlacementSpec, PlacementWrite, PluginPlacement, Replicas, ScheduleReport,
    HOST_LABEL, MAX_HOST_LABELS, MAX_PLACEMENT_CONSTRAINTS, MAX_REPLICAS, SCHEDULER_LEASE,
    SCHEDULER_LEASE_TTL, TARGET_LABEL,
};
pub use releases::{
    activate_release, approve_release_grant, clear_active_release, list_release_pointers,
    list_releases, load_active_release, load_release, load_release_blob, load_release_pointer,
    publish_release, put_release_blob, PluginRelease, PublishRelease, ReleaseArtifact,
    ReleasePointer, MAX_RELEASE_BLOB_BYTES, RELEASE_BLOB_SCHEME,
};
pub use schemas::{
    list_plugin_schemas, load_plugin_schema, pin_plugin_schema, pin_release_schema,
    require_plugin_schema, schema_fingerprint, stale_against_pin, PinnedPluginSchema, SchemaPin,
    SchemaPinChange, SchemaPinOutcome,
};
pub use secret::{align_cluster_root, load_cluster_row, ClusterSecret};

use documents::{import_if_absent, load_document, replace_document};
use identity::heartbeat_process;

use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace for the migrated `[events]` domain.
pub const EVENTS_NAMESPACE: &str = "core.events";

/// Namespace for host-scoped runtime settings.
pub const HOST_RUNTIME_NAMESPACE: &str = "host.runtime";

/// Schema version of [`EventsSettingsV1`].
pub const EVENTS_SCHEMA_VERSION: i64 = 1;

/// Schema version of [`HostRuntimeSettingsV1`].
pub const HOST_RUNTIME_SCHEMA_VERSION: i64 = 1;

/// Upper bound for event retention fields, in days.
pub const MAX_EVENTS_RETENTION_DAYS: u64 = 3650;

/// Upper bound for `[events].concurrency`.
pub const MAX_EVENTS_CONCURRENCY: u32 = 32;

/// Typed `[events]` document, schema version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsSettingsV1 {
    /// Days to keep acked and rejected deliveries.
    pub retention_days: u64,
    /// Days to keep dead-lettered deliveries.
    pub dead_letter_retention_days: u64,
    /// Local delivery workers and the cluster in-flight cap.
    pub concurrency: u32,
}

impl EventsSettingsV1 {
    /// Rejects values outside the domain bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is outside its allowed range.
    pub fn validate(&self) -> Result<()> {
        if !(1..=MAX_EVENTS_RETENTION_DAYS).contains(&self.retention_days) {
            return Err(invalid_events("retention_days must be 1..=3650"));
        }
        if !(1..=MAX_EVENTS_RETENTION_DAYS).contains(&self.dead_letter_retention_days) {
            return Err(invalid_events(
                "dead_letter_retention_days must be 1..=3650",
            ));
        }
        if !(1..=MAX_EVENTS_CONCURRENCY).contains(&self.concurrency) {
            return Err(invalid_events("concurrency must be 1..=32"));
        }
        Ok(())
    }

    /// Copies the transitional `[events]` table, rejecting values the domain forbids.
    ///
    /// # Errors
    ///
    /// Returns an error when the table fails [`Self::validate`].
    pub fn from_config(config: &bookclerk_config::EventsConfig) -> Result<Self> {
        let settings = Self {
            retention_days: config.retention_days,
            dead_letter_retention_days: config.dead_letter_retention_days,
            concurrency: config.concurrency,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Writes the document onto the in-memory events table.
    pub fn apply_to(&self, config: &mut bookclerk_config::EventsConfig) {
        config.retention_days = self.retention_days;
        config.dead_letter_retention_days = self.dead_letter_retention_days;
        config.concurrency = self.concurrency;
    }
}

/// Typed host-scoped runtime document, schema version 1.
///
/// Desired state for one host, editable from any host in the cluster: an
/// operator label, and the budget the scheduler may commit on this host.
/// Budgets are shares of the capacity the host detects at heartbeat
/// (CPU cores and memory, the tighter of the machine and its cgroup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRuntimeSettingsV1 {
    /// Operator label shown for this host. Empty is allowed.
    pub label: String,
    /// Share of detected CPU cores the scheduler may commit to plugin
    /// guests on this host (1..=100).
    #[serde(default = "default_cpu_budget_percent")]
    pub cpu_budget_percent: u8,
    /// Share of detected memory the scheduler may commit (1..=100).
    #[serde(default = "default_memory_budget_percent")]
    pub memory_budget_percent: u8,
    /// False drains the host: the scheduler places nothing new here and
    /// moves its replicas elsewhere. Hand-managed deployments still run.
    #[serde(default = "default_schedulable")]
    pub schedulable: bool,
}

/// Default CPU budget: leave a fifth of the cores to the rest of the host.
pub const DEFAULT_CPU_BUDGET_PERCENT: u8 = 80;

/// Default memory budget: leave a quarter of the memory to the rest of the host.
pub const DEFAULT_MEMORY_BUDGET_PERCENT: u8 = 75;

/// Serde default for [`HostRuntimeSettingsV1::cpu_budget_percent`].
fn default_cpu_budget_percent() -> u8 {
    DEFAULT_CPU_BUDGET_PERCENT
}

/// Serde default for [`HostRuntimeSettingsV1::memory_budget_percent`].
fn default_memory_budget_percent() -> u8 {
    DEFAULT_MEMORY_BUDGET_PERCENT
}

/// Serde default for [`HostRuntimeSettingsV1::schedulable`].
fn default_schedulable() -> bool {
    true
}

impl HostRuntimeSettingsV1 {
    /// Empty label, default budgets, schedulable.
    #[must_use]
    pub fn new() -> Self {
        Self {
            label: String::new(),
            cpu_budget_percent: DEFAULT_CPU_BUDGET_PERCENT,
            memory_budget_percent: DEFAULT_MEMORY_BUDGET_PERCENT,
            schedulable: true,
        }
    }

    /// Rejects control characters, labels longer than 64 bytes, and budgets
    /// outside 1..=100.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is out of range.
    pub fn validate(&self) -> Result<()> {
        if self.label.len() > 64 {
            return Err(invalid_events(
                "host runtime label must be at most 64 bytes",
            ));
        }
        if self.label.chars().any(|ch| ch.is_control()) {
            return Err(invalid_events(
                "host runtime label must not contain control characters",
            ));
        }
        for (name, percent) in [
            ("cpu_budget_percent", self.cpu_budget_percent),
            ("memory_budget_percent", self.memory_budget_percent),
        ] {
            if !(1..=100).contains(&percent) {
                return Err(invalid_events(&format!("{name} must be between 1 and 100")));
            }
        }
        Ok(())
    }
}

impl Default for HostRuntimeSettingsV1 {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed domain document plus its revision metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationDocument<T> {
    /// Parsed body.
    pub body: T,
    /// Compare-and-swap revision.
    pub revision: i64,
    /// Domain schema version.
    pub schema_version: i64,
    /// RFC 3339 commit time.
    pub updated_at: String,
    /// Audit identity of the last writer.
    pub updated_by: String,
    /// Scope type (`cluster` or `host`).
    pub scope_type: String,
    /// Scope id (`singleton` or a host id).
    pub scope_id: String,
    /// Domain namespace.
    pub namespace: String,
}

/// Identity, secret root, and the authoritative events document for this process.
#[derive(Debug, Clone)]
pub struct ControlPlaneSession {
    /// Registered host.
    pub host: HostRecord,
    /// Cluster id from the secret root.
    pub cluster_id: String,
    /// Authoritative `[events]` document.
    pub events: ConfigurationDocument<EventsSettingsV1>,
    /// This host's runtime document. Other hosts' documents are not included.
    pub host_runtime: ConfigurationDocument<HostRuntimeSettingsV1>,
}

/// Enrolls this files directory and imports `[events]` when the domain is absent.
///
/// After this returns, callers must treat [`ControlPlaneSession::events`] as
/// authority. A later TOML load or environment override must not replace it
/// without a compare-and-swap write.
///
/// `scratch_dir` is [`bookclerk_config::Config::download_cache_dir`]: the
/// heartbeat records the byte size of its `acquire` and `acquire-pdf` trees.
///
/// # Errors
///
/// Returns an error when the secret root, cluster binding, schema, or events
/// document cannot be established.
pub async fn bootstrap_control_plane(
    store: &LibraryStore,
    files_dir: &Path,
    scratch_dir: &Path,
    password: Option<&str>,
    events_seed: &bookclerk_config::EventsConfig,
) -> Result<ControlPlaneSession> {
    let _identity = identity::load_or_create_host_identity(files_dir)?;
    let secret = align_cluster_root(store, files_dir, password).await?;
    let identity = identity::bind_cluster_id(files_dir, &secret.cluster_id)?;
    let host = heartbeat_process(
        store,
        &identity.host_id,
        &secret.cluster_id,
        files_dir,
        scratch_dir,
    )
    .await?;
    tracing::info!(
        host_id = %host.host_id,
        logical_cpus = ?host.logical_cpus,
        cpu_max_quota_us = ?host.cpu_max_quota_us,
        cpu_max_period_us = ?host.cpu_max_period_us,
        memory_max_bytes = ?host.memory_max_bytes,
        memory_current_bytes = ?host.memory_current_bytes,
        memory_anon_bytes = ?host.memory_anon_bytes,
        files_dir_free_bytes = ?host.files_dir_free_bytes,
        scratch_bytes = ?host.scratch_bytes,
        "host capacity observation"
    );
    let events = import_events_if_absent(
        store,
        &ConfigActor::Bootstrap,
        events_seed,
        "bootstrap-core-events",
    )
    .await?;
    let host_runtime = import_host_runtime_if_absent(
        store,
        &ConfigActor::Bootstrap,
        &host.host_id,
        &HostRuntimeSettingsV1::new(),
        &format!("bootstrap-host-runtime-{}", host.host_id),
    )
    .await?;
    Ok(ControlPlaneSession {
        host,
        cluster_id: secret.cluster_id,
        events,
        host_runtime,
    })
}

/// Copies the authoritative events document onto `config` for `cluster_id`.
///
/// Revisions are only comparable within one cluster. Callers that switch
/// databases pass the new cluster id explicitly.
pub fn overlay_events(
    config: &mut bookclerk_config::Config,
    events: &ConfigurationDocument<EventsSettingsV1>,
    cluster_id: &str,
) {
    events.body.apply_to(&mut config.events);
    config.events_revision = Some(events.revision);
    config.events_authority = Some(cluster_id.to_string());
}

/// Loads the cluster events document.
///
/// # Errors
///
/// Returns an error when the document is missing, unsupported, or invalid.
pub async fn load_events(store: &LibraryStore) -> Result<ConfigurationDocument<EventsSettingsV1>> {
    let stored = load_document(store, &DocumentKey::cluster(EVENTS_NAMESPACE))
        .await?
        .ok_or_else(|| {
            LibraryError::NotFound("configuration core.events is not initialized".into())
        })?;
    parse_events(stored)
}

/// Inserts `[events]` when the cluster document is absent.
///
/// An existing document is parsed and returned without reading the seed, so an
/// obsolete or out-of-range TOML/environment value cannot fail startup.
///
/// # Errors
///
/// Returns an error when the stored document is invalid, or when the document
/// is absent and the seed is invalid or the actor may not import.
pub async fn import_events_if_absent(
    store: &LibraryStore,
    actor: &ConfigActor,
    seed: &bookclerk_config::EventsConfig,
    operation_id: &str,
) -> Result<ConfigurationDocument<EventsSettingsV1>> {
    let key = DocumentKey::cluster(EVENTS_NAMESPACE);
    if let Some(existing) = load_document(store, &key).await? {
        return parse_events(existing);
    }
    let body = match EventsSettingsV1::from_config(seed) {
        Ok(body) => body,
        Err(err) => {
            // A concurrent initializer may have committed while this seed was
            // rejected. The stored document wins; the seed is not applied.
            if let Some(existing) = load_document(store, &key).await? {
                return parse_events(existing);
            }
            return Err(err);
        }
    };
    let json = serde_json::to_string(&body)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid configuration: {err}")))?;
    let stored = import_if_absent(
        store,
        actor,
        &key,
        EVENTS_SCHEMA_VERSION,
        &json,
        operation_id,
    )
    .await?;
    parse_events(stored)
}

/// Replaces the cluster events document.
///
/// # Errors
///
/// Returns an error when validation, authorization, or the compare-and-swap fails.
pub async fn replace_events(
    store: &LibraryStore,
    actor: &ConfigActor,
    expected_revision: i64,
    body: &EventsSettingsV1,
    operation_id: &str,
) -> Result<EventsReplace> {
    body.validate()?;
    let json = serde_json::to_string(body)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid configuration: {err}")))?;
    match replace_document(
        store,
        actor,
        &DocumentKey::cluster(EVENTS_NAMESPACE),
        EVENTS_SCHEMA_VERSION,
        expected_revision,
        &json,
        operation_id,
    )
    .await?
    {
        ReplaceOutcome::Applied(stored) => Ok(EventsReplace::Applied(parse_events(stored)?)),
        ReplaceOutcome::Conflict { current_revision } => {
            Ok(EventsReplace::Conflict { current_revision })
        }
        ReplaceOutcome::Replayed { revision } => Ok(EventsReplace::Replayed { revision }),
    }
}

/// Outcome of [`replace_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventsReplace {
    /// The new document is committed.
    Applied(ConfigurationDocument<EventsSettingsV1>),
    /// `expected_revision` was not current. The stored document is unchanged.
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
    /// This operation id already committed at `revision`.
    Replayed {
        /// Revision of the original commit.
        revision: i64,
    },
}

/// Loads one host's runtime document.
///
/// # Errors
///
/// Returns an error when the document is missing, unsupported, or invalid.
pub async fn load_host_runtime(
    store: &LibraryStore,
    host_id: &str,
) -> Result<ConfigurationDocument<HostRuntimeSettingsV1>> {
    let stored = load_document(store, &DocumentKey::host(host_id, HOST_RUNTIME_NAMESPACE))
        .await?
        .ok_or_else(|| {
            LibraryError::NotFound(format!(
                "configuration host.runtime for {host_id} is not initialized"
            ))
        })?;
    parse_host_runtime(stored)
}

/// Inserts a host runtime document when it is absent.
///
/// # Errors
///
/// Returns an error when the body is invalid or the actor may not import.
pub async fn import_host_runtime_if_absent(
    store: &LibraryStore,
    actor: &ConfigActor,
    host_id: &str,
    body: &HostRuntimeSettingsV1,
    operation_id: &str,
) -> Result<ConfigurationDocument<HostRuntimeSettingsV1>> {
    body.validate()?;
    let json = serde_json::to_string(body)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid configuration: {err}")))?;
    let stored = import_if_absent(
        store,
        actor,
        &DocumentKey::host(host_id, HOST_RUNTIME_NAMESPACE),
        HOST_RUNTIME_SCHEMA_VERSION,
        &json,
        operation_id,
    )
    .await?;
    parse_host_runtime(stored)
}

/// Replaces one host's runtime document.
///
/// # Errors
///
/// Returns an error when validation, authorization, or the compare-and-swap fails.
pub async fn replace_host_runtime(
    store: &LibraryStore,
    actor: &ConfigActor,
    host_id: &str,
    expected_revision: i64,
    body: &HostRuntimeSettingsV1,
    operation_id: &str,
) -> Result<HostRuntimeReplace> {
    body.validate()?;
    let json = serde_json::to_string(body)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid configuration: {err}")))?;
    match replace_document(
        store,
        actor,
        &DocumentKey::host(host_id, HOST_RUNTIME_NAMESPACE),
        HOST_RUNTIME_SCHEMA_VERSION,
        expected_revision,
        &json,
        operation_id,
    )
    .await?
    {
        ReplaceOutcome::Applied(stored) => {
            Ok(HostRuntimeReplace::Applied(parse_host_runtime(stored)?))
        }
        ReplaceOutcome::Conflict { current_revision } => {
            Ok(HostRuntimeReplace::Conflict { current_revision })
        }
        ReplaceOutcome::Replayed { revision } => Ok(HostRuntimeReplace::Replayed { revision }),
    }
}

/// Applies `edit` to a host's runtime document (creating it with defaults
/// when the host never wrote one) under compare-and-swap, retrying a few
/// times on a concurrent edit. Budgets reach the scheduler with the host's
/// next heartbeat.
///
/// # Errors
///
/// Returns an error when the actor may not write, the edited body is
/// invalid, or the document keeps changing underneath.
pub async fn update_host_runtime(
    store: &LibraryStore,
    actor: &ConfigActor,
    host_id: &str,
    edit: impl Fn(&mut HostRuntimeSettingsV1),
) -> Result<ConfigurationDocument<HostRuntimeSettingsV1>> {
    for attempt in 0..4 {
        let current = match load_host_runtime(store, host_id).await {
            Ok(document) => document,
            Err(LibraryError::NotFound(_)) => {
                import_host_runtime_if_absent(
                    store,
                    actor,
                    host_id,
                    &HostRuntimeSettingsV1::new(),
                    &format!("host-runtime-default-{host_id}"),
                )
                .await?
            }
            Err(err) => return Err(err),
        };
        let mut body = current.body.clone();
        edit(&mut body);
        if body == current.body {
            return Ok(current);
        }
        let operation_id = format!(
            "host-runtime-{host_id}-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_host_runtime(
            store,
            actor,
            host_id,
            current.revision,
            &body,
            &operation_id,
        )
        .await?
        {
            HostRuntimeReplace::Applied(document) => return Ok(document),
            HostRuntimeReplace::Conflict { .. } | HostRuntimeReplace::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(format!(
        "host.runtime for {host_id} kept changing; retry"
    )))
}

/// Outcome of [`replace_host_runtime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRuntimeReplace {
    /// The new document is committed.
    Applied(ConfigurationDocument<HostRuntimeSettingsV1>),
    /// `expected_revision` was not current.
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
    /// This operation id already committed at `revision`.
    Replayed {
        /// Revision of the original commit.
        revision: i64,
    },
}

/// Parses a stored `core.events` row, failing closed on an unknown schema.
fn parse_events(stored: StoredDocument) -> Result<ConfigurationDocument<EventsSettingsV1>> {
    if stored.schema_version != EVENTS_SCHEMA_VERSION {
        return Err(documents::unsupported(
            EVENTS_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: EventsSettingsV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid configuration: {err}")))?;
    body.validate()?;
    Ok(document(stored, body))
}

/// Parses a stored `host.runtime` row, failing closed on an unknown schema.
fn parse_host_runtime(
    stored: StoredDocument,
) -> Result<ConfigurationDocument<HostRuntimeSettingsV1>> {
    if stored.schema_version != HOST_RUNTIME_SCHEMA_VERSION {
        return Err(documents::unsupported(
            HOST_RUNTIME_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: HostRuntimeSettingsV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("invalid configuration: {err}")))?;
    body.validate()?;
    Ok(document(stored, body))
}

/// Attaches revision metadata to a parsed body.
fn document<T>(stored: StoredDocument, body: T) -> ConfigurationDocument<T> {
    ConfigurationDocument {
        body,
        revision: stored.revision,
        schema_version: stored.schema_version,
        updated_at: stored.updated_at,
        updated_by: stored.updated_by,
        scope_type: stored.key.scope_type,
        scope_id: stored.key.scope_id,
        namespace: stored.key.namespace,
    }
}

/// Error for an events or host-runtime value outside the domain bounds.
fn invalid_events(detail: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("invalid configuration: {detail}"))
}

#[cfg(test)]
mod instance_tests;
#[cfg(test)]
mod placement_tests;
#[cfg(test)]
mod tests;
