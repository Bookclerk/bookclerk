//! Database-backed control plane: node identity and typed configuration.
//!
//! Bootstrap material (database target, `master.key`, `node-identity.json`) is
//! read before these rows exist. After enrollment, `core.events` is the
//! authority for `[events]`. Other `config.toml` sections stay transitional.
//!
//! See `docs/adr/control-plane.md`.

mod batch;
mod deployments;
mod documents;
mod enrollment;
mod identity;
mod instance_config;
mod instances;
mod job_routing;
mod leases;
mod node_queue;
mod node_status;
mod ownership;
mod placement;
mod requirements_sweep;
mod routing;
mod schemas;
mod secret;
mod semaphore;
mod versions;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use deployments::{
    bounded_observation_detail, delete_plugin_deployment, ensure_plugin_deployment,
    instance_has_deployment, list_all_deployments, list_deployments_for_instance,
    list_deployments_for_node, list_observations_for_deployment, list_present_deployments_for_node,
    load_deployment, load_deployment_for_node, load_observation, present_plugin_keys_for_node,
    replace_plugin_deployment, upsert_observation, DeploymentObservation, DeploymentReplace,
    DeploymentStatus, DesiredState, PluginDeployment, MAX_OBSERVATION_DETAIL_BYTES,
};
pub use documents::{
    audit_count, change_count, ConfigActor, DocumentKey, ReplaceOutcome, StoredDocument,
    VerifiedClusterOwner, VerifiedNodeOwner, CLUSTER_SCOPE_ID, CLUSTER_SCOPE_TYPE,
    MAX_CONFIGURATION_DOCUMENT_BYTES, NODE_SCOPE_TYPE, PLUGIN_INSTANCE_CONFIG_NAMESPACE,
    PLUGIN_INSTANCE_SCOPE_TYPE,
};
pub use enrollment::{
    format_enrollment_token, list_enrollment_links, load_host_enrollment, load_node_credential,
    mint_enrollment_link, parse_enrollment_token, redeem_enrollment_link, revoke_enrollment_link,
    revoke_node_credential, set_host_enrollment_enabled, verify_cluster_owner, EnrolledNode,
    EnrollmentActor, EnrollmentLink, EnrollmentSecret, EnrollmentToken, HostEnrollmentSettings,
    HostEnrollmentSettingsV1, MintedLink, NodeCredential, RedeemRequest, DEFAULT_LINK_TTL,
    ENROLLMENT_TOKEN_PREFIX, HOST_ENROLLMENT_NAMESPACE, HOST_ENROLLMENT_SCHEMA_VERSION,
    MAX_LINK_LABEL_BYTES, MAX_LINK_TTL, MIN_LINK_TTL, NODE_PUBLIC_KEY_LEN,
};
pub use identity::{
    current_process_incarnation, list_node_ids, list_nodes, load_node,
    load_or_create_node_identity, node_identity_path, node_target, NodeIdentity, NodeKind,
    NodeRecord, NODE_IDENTITY_FILE,
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
pub use job_routing::{
    account_semaphore, claim_routed_job, class_semaphore, common_deployment_nodes,
    instance_semaphore, job_semaphores, node_can_run_job, resolve_job_requirements, JobClaimLimits,
    JobClaimRequest, RoutedJobClaim, ACCOUNT_SEMAPHORE_LIMIT, ROUTED_CLAIM_CANDIDATES,
};
pub use leases::{
    acquire_lease, acquire_lease_on, list_leases, load_lease, load_lease_on, release_lease,
    release_lease_on, renew_lease, renew_lease_on, ClusterLease, LeaseFence, SingletonDuty,
    EVENT_RETENTION_LEASE, JOB_RETENTION_LEASE, KV_EXPIRY_LEASE, MIGRATION_LEASE,
    SESSION_EXPIRY_LEASE,
};
pub use node_queue::{
    is_node_consumer_id, is_valid_node_id, node_consumer_id, node_queue_name, publish_node_message,
    publish_to_all_nodes, NodeMessage, NODE_CONSUMER_PREFIX,
};
pub use node_status::{
    instance_demand, list_node_statuses, load_node_status, next_availability, node_reservations,
    record_deployment_reservation, record_node_heartbeat, GuestReservation, NodeHeartbeat,
    NodeSampler, NodeStatus, AVAILABILITY_FULL_PPM, AVAILABILITY_TAU, DEFAULT_GUEST_CPU_MILLICORES,
    DEFAULT_GUEST_MEMORY_BYTES, NODE_HEARTBEAT_INTERVAL, NODE_LIVE_INTERVALS,
};
pub use ownership::{nodes_owned_by, set_node_owner, verify_node_owner};
pub use placement::undeploy_instance;
pub use placement::{
    delete_plugin_placement, effective_node_labels, ensure_hand_managed, labels_satisfy,
    list_plugin_placements, live_node_ids, load_plugin_placement, plan_placement,
    put_plugin_placement, schedule_placements, set_node_labels, validate_constraint,
    validate_label, validate_node_label, NodeCandidate, PlacementPlan, PlacementSpec,
    PlacementWrite, PluginPlacement, Replicas, ScheduleReport, MAX_NODE_LABELS,
    MAX_PLACEMENT_CONSTRAINTS, MAX_REPLICAS, NODE_LABEL, SCHEDULER_LEASE, SCHEDULER_LEASE_TTL,
    TARGET_LABEL,
};
pub use requirements_sweep::{
    sweep_job_requirements, RequirementsSweepReport, REQUIREMENTS_GRACE, REQUIREMENTS_SWEEP_LEASE,
    REQUIREMENTS_SWEEP_LEASE_TTL, UNROUTABLE_ERROR_KIND,
};
pub use routing::{rank_nodes, route_ranks, RouteRank, ROUTE_DEFER_PER_RANK};
pub use schemas::{
    list_plugin_schemas, load_plugin_schema, pin_plugin_schema, pin_version_schema,
    require_plugin_schema, schema_fingerprint, stale_against_pin, PinnedPluginSchema, SchemaPin,
    SchemaPinChange, SchemaPinOutcome,
};
pub use secret::{align_cluster_root, load_cluster_row, ClusterSecret};
pub use semaphore::{
    acquire_permit, acquire_permits, live_permits, release_permits, renew_fenced_lease,
    renew_permits, semaphore_slot_lease_name, PermitAcquire, SemaphorePermit, SemaphoreRequest,
    MAX_SEMAPHORE_LIMIT, SEMAPHORE_LEASE_PREFIX,
};
pub use versions::{
    activate_version, add_plugin_version, approve_version_grant, clear_install, list_installs,
    list_versions, load_active_version, load_artifact_blob, load_install, load_version,
    put_artifact_blob, AddVersion, PluginArtifact, PluginInstall, PluginVersion,
    ARTIFACT_BLOB_SCHEME, MAX_ARTIFACT_BLOB_BYTES,
};

use documents::{import_if_absent, load_document, replace_document};
use identity::heartbeat_process;

use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace for the migrated `[events]` domain.
pub const EVENTS_NAMESPACE: &str = "core.events";

/// Namespace for node-scoped runtime settings.
pub const NODE_RUNTIME_NAMESPACE: &str = "node.runtime";

/// Schema version of [`EventsSettingsV1`].
pub const EVENTS_SCHEMA_VERSION: i64 = 1;

/// Schema version of [`NodeRuntimeSettingsV1`].
pub const NODE_RUNTIME_SCHEMA_VERSION: i64 = 1;

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

/// Typed node-scoped runtime document, schema version 1.
///
/// Desired state for one node, editable from any node in the cluster: an
/// operator label, and the budget the scheduler may commit on this node.
/// Budgets are shares of the capacity the node detects at heartbeat
/// (CPU cores and memory, the tighter of the machine and its cgroup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRuntimeSettingsV1 {
    /// Operator label shown for this node. Empty is allowed.
    pub label: String,
    /// Share of detected CPU cores the scheduler may commit to plugin
    /// guests on this node (1..=100).
    #[serde(default = "default_cpu_budget_percent")]
    pub cpu_budget_percent: u8,
    /// Share of detected memory the scheduler may commit (1..=100).
    #[serde(default = "default_memory_budget_percent")]
    pub memory_budget_percent: u8,
    /// False drains the node: the scheduler places nothing new here and
    /// moves its replicas elsewhere. Hand-managed deployments still run.
    #[serde(default = "default_schedulable")]
    pub schedulable: bool,
}

/// Default CPU budget: leave a fifth of the cores to the rest of the node.
pub const DEFAULT_CPU_BUDGET_PERCENT: u8 = 80;

/// Default memory budget: leave a quarter of the memory to the rest of the node.
pub const DEFAULT_MEMORY_BUDGET_PERCENT: u8 = 75;

/// Serde default for [`NodeRuntimeSettingsV1::cpu_budget_percent`].
fn default_cpu_budget_percent() -> u8 {
    DEFAULT_CPU_BUDGET_PERCENT
}

/// Serde default for [`NodeRuntimeSettingsV1::memory_budget_percent`].
fn default_memory_budget_percent() -> u8 {
    DEFAULT_MEMORY_BUDGET_PERCENT
}

/// Serde default for [`NodeRuntimeSettingsV1::schedulable`].
fn default_schedulable() -> bool {
    true
}

impl NodeRuntimeSettingsV1 {
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
                "node runtime label must be at most 64 bytes",
            ));
        }
        if self.label.chars().any(|ch| ch.is_control()) {
            return Err(invalid_events(
                "node runtime label must not contain control characters",
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

impl Default for NodeRuntimeSettingsV1 {
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
    /// Scope type (`cluster` or `node`).
    pub scope_type: String,
    /// Scope id (`singleton` or a node id).
    pub scope_id: String,
    /// Domain namespace.
    pub namespace: String,
}

/// Identity, secret root, and the authoritative events document for this process.
#[derive(Debug, Clone)]
pub struct ControlPlaneSession {
    /// Registered node.
    pub node: NodeRecord,
    /// Cluster id from the secret root.
    pub cluster_id: String,
    /// Authoritative `[events]` document.
    pub events: ConfigurationDocument<EventsSettingsV1>,
    /// This node's runtime document. Other nodes' documents are not included.
    pub node_runtime: ConfigurationDocument<NodeRuntimeSettingsV1>,
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
    let _identity = identity::load_or_create_node_identity(files_dir)?;
    let secret = align_cluster_root(store, files_dir, password).await?;
    let identity = identity::bind_cluster_id(files_dir, &secret.cluster_id)?;
    let node = heartbeat_process(
        store,
        &identity.node_id,
        &secret.cluster_id,
        files_dir,
        scratch_dir,
    )
    .await?;
    tracing::info!(
        node_id = %node.node_id,
        logical_cpus = ?node.logical_cpus,
        cpu_max_quota_us = ?node.cpu_max_quota_us,
        cpu_max_period_us = ?node.cpu_max_period_us,
        memory_max_bytes = ?node.memory_max_bytes,
        memory_current_bytes = ?node.memory_current_bytes,
        memory_anon_bytes = ?node.memory_anon_bytes,
        files_dir_free_bytes = ?node.files_dir_free_bytes,
        scratch_bytes = ?node.scratch_bytes,
        "node capacity observation"
    );
    let events = import_events_if_absent(
        store,
        &ConfigActor::Bootstrap,
        events_seed,
        "bootstrap-core-events",
    )
    .await?;
    let node_runtime = import_node_runtime_if_absent(
        store,
        &ConfigActor::Bootstrap,
        &node.node_id,
        &NodeRuntimeSettingsV1::new(),
        &format!("bootstrap-node-runtime-{}", node.node_id),
    )
    .await?;
    Ok(ControlPlaneSession {
        node,
        cluster_id: secret.cluster_id,
        events,
        node_runtime,
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
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
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
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
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

/// Loads one node's runtime document.
///
/// # Errors
///
/// Returns an error when the document is missing, unsupported, or invalid.
pub async fn load_node_runtime(
    store: &LibraryStore,
    node_id: &str,
) -> Result<ConfigurationDocument<NodeRuntimeSettingsV1>> {
    let stored = load_document(store, &DocumentKey::node(node_id, NODE_RUNTIME_NAMESPACE))
        .await?
        .ok_or_else(|| {
            LibraryError::NotFound(format!(
                "configuration node.runtime for {node_id} is not initialized"
            ))
        })?;
    parse_node_runtime(stored)
}

/// Inserts a node runtime document when it is absent.
///
/// # Errors
///
/// Returns an error when the body is invalid or the actor may not import.
pub async fn import_node_runtime_if_absent(
    store: &LibraryStore,
    actor: &ConfigActor,
    node_id: &str,
    body: &NodeRuntimeSettingsV1,
    operation_id: &str,
) -> Result<ConfigurationDocument<NodeRuntimeSettingsV1>> {
    body.validate()?;
    let json = serde_json::to_string(body)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    let stored = import_if_absent(
        store,
        actor,
        &DocumentKey::node(node_id, NODE_RUNTIME_NAMESPACE),
        NODE_RUNTIME_SCHEMA_VERSION,
        &json,
        operation_id,
    )
    .await?;
    parse_node_runtime(stored)
}

/// Replaces one node's runtime document.
///
/// # Errors
///
/// Returns an error when validation, authorization, or the compare-and-swap fails.
pub async fn replace_node_runtime(
    store: &LibraryStore,
    actor: &ConfigActor,
    node_id: &str,
    expected_revision: i64,
    body: &NodeRuntimeSettingsV1,
    operation_id: &str,
) -> Result<NodeRuntimeReplace> {
    body.validate()?;
    let json = serde_json::to_string(body)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    match replace_document(
        store,
        actor,
        &DocumentKey::node(node_id, NODE_RUNTIME_NAMESPACE),
        NODE_RUNTIME_SCHEMA_VERSION,
        expected_revision,
        &json,
        operation_id,
    )
    .await?
    {
        ReplaceOutcome::Applied(stored) => {
            Ok(NodeRuntimeReplace::Applied(parse_node_runtime(stored)?))
        }
        ReplaceOutcome::Conflict { current_revision } => {
            Ok(NodeRuntimeReplace::Conflict { current_revision })
        }
        ReplaceOutcome::Replayed { revision } => Ok(NodeRuntimeReplace::Replayed { revision }),
    }
}

/// Applies `edit` to a node's runtime document (creating it with defaults
/// when the node never wrote one) under compare-and-swap, retrying a few
/// times on a concurrent edit. Budgets reach the scheduler with the node's
/// next heartbeat.
///
/// # Errors
///
/// Returns an error when the actor may not write, the edited body is
/// invalid, or the document keeps changing underneath.
pub async fn update_node_runtime(
    store: &LibraryStore,
    actor: &ConfigActor,
    node_id: &str,
    edit: impl Fn(&mut NodeRuntimeSettingsV1),
) -> Result<ConfigurationDocument<NodeRuntimeSettingsV1>> {
    for attempt in 0..4 {
        let current = match load_node_runtime(store, node_id).await {
            Ok(document) => document,
            Err(LibraryError::NotFound(_)) => {
                import_node_runtime_if_absent(
                    store,
                    actor,
                    node_id,
                    &NodeRuntimeSettingsV1::new(),
                    &format!("node-runtime-default-{node_id}"),
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
            "node-runtime-{node_id}-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_node_runtime(
            store,
            actor,
            node_id,
            current.revision,
            &body,
            &operation_id,
        )
        .await?
        {
            NodeRuntimeReplace::Applied(document) => return Ok(document),
            NodeRuntimeReplace::Conflict { .. } | NodeRuntimeReplace::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(format!(
        "node.runtime for {node_id} kept changing; retry"
    )))
}

/// Outcome of [`replace_node_runtime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRuntimeReplace {
    /// The new document is committed.
    Applied(ConfigurationDocument<NodeRuntimeSettingsV1>),
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
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    body.validate()?;
    Ok(document(stored, body))
}

/// Parses a stored `node.runtime` row, failing closed on an unknown schema.
fn parse_node_runtime(
    stored: StoredDocument,
) -> Result<ConfigurationDocument<NodeRuntimeSettingsV1>> {
    if stored.schema_version != NODE_RUNTIME_SCHEMA_VERSION {
        return Err(documents::unsupported(
            NODE_RUNTIME_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: NodeRuntimeSettingsV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
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

/// Error for an events or node-runtime value outside the domain bounds.
fn invalid_events(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid configuration: {detail}"))
}

#[cfg(test)]
mod enrollment_tests;
#[cfg(test)]
mod instance_tests;
#[cfg(test)]
mod job_routing_tests;
#[cfg(test)]
mod leases_tests;
#[cfg(test)]
mod placement_tests;
#[cfg(test)]
mod requirements_sweep_tests;
#[cfg(test)]
mod tests;
