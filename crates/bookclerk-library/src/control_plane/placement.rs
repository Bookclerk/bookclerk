//! Placement: how many nodes run a plugin instance, and which nodes may.
//!
//! An operator writes intent per instance in `plugin_placements`: a replica
//! count and label constraints. The scheduler, a singleton that holds the
//! [`SCHEDULER_LEASE`] cluster lease, turns that intent into
//! `plugin_deployments` rows (the same rows `plugins deploy` writes by
//! hand), and each node's reconciler converges on its own rows as before.
//! An instance with no placement row is managed by hand; the scheduler never
//! touches it.
//!
//! A node is a candidate for an instance when:
//!
//! - its daemon is live: its `node:<node_id>` queue consumer heartbeated
//!   within the event catalog TTL;
//! - its labels satisfy every constraint (`name` or `name=value` must be
//!   present, `!name…` must be absent). Every node also carries the implicit
//!   labels `node=<node_id>` and `target=<target>`;
//! - for a released plugin, the active release has an artifact for the
//!   node's target.
//!
//! The planner keeps replicas where they already run, drops replicas on
//! nodes that stopped qualifying (a dead node, a removed label), and fills
//! the rest on the least-loaded candidates. Deployment writes are
//! compare-and-swap, so a scheduler that lost its lease mid-pass can at worst
//! place one replica too many, which the next pass removes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::deployments::{
    ensure_plugin_deployment, list_all_deployments, list_deployments_for_instance,
    replace_plugin_deployment, DeploymentReplace, DesiredState, PluginDeployment,
};
use super::documents::ConfigActor;
use super::identity::{list_nodes, load_node, NodeRecord};
use super::instances::{load_plugin_instance, PluginInstanceId};
use super::node_queue::{notify_all_nodes, NodeMessage};
use super::node_status::{GuestReservation, NodeStatus};
use super::versions::load_active_version;
use crate::entities::{cluster_leases, nodes, plugin_placements};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Name of the cluster lease the scheduler runs under.
pub const SCHEDULER_LEASE: &str = "scheduler";

/// How long a scheduler lease lasts without renewal.
pub const SCHEDULER_LEASE_TTL: Duration = Duration::from_secs(60);

/// Largest replica count an operator may ask for.
pub const MAX_REPLICAS: u32 = 64;

/// Most operator labels one node may carry.
pub const MAX_NODE_LABELS: usize = 32;

/// Most constraints one placement may carry.
pub const MAX_PLACEMENT_CONSTRAINTS: usize = 16;

/// Longest label or constraint (without its `!`), in bytes.
const MAX_LABEL_BYTES: usize = 63;

/// Implicit label name every node carries with its own id.
pub const NODE_LABEL: &str = "node";

/// Implicit label name every node carries with its artifact target.
pub const TARGET_LABEL: &str = "target";

/// How many nodes should run a placed instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Replicas {
    /// Exactly this many nodes (zero keeps the placement and stops every
    /// replica).
    Count(u32),
    /// Every node that qualifies and has room; calls then go to the best of
    /// them. Suits nodes that come and go.
    All,
}

impl Default for Replicas {
    fn default() -> Self {
        Self::Count(1)
    }
}

impl std::fmt::Display for Replicas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(n) => write!(f, "{n}"),
            Self::All => f.write_str("all"),
        }
    }
}

impl std::str::FromStr for Replicas {
    type Err = LibraryError;

    fn from_str(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        raw.parse::<u32>()
            .map(Self::Count)
            .map_err(|_| invalid("replicas must be a number or `all`"))
    }
}

impl serde::Serialize for Replicas {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Count(n) => serializer.serialize_u32(*n),
            Self::All => serializer.serialize_str("all"),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Replicas {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            /// A count.
            Count(u32),
            /// `"all"` or a numeric string.
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Count(n) => Ok(Self::Count(n)),
            Raw::Text(text) => text.parse().map_err(serde::de::Error::custom),
        }
    }
}

impl Replicas {
    /// Stored column value (`NULL` for [`Replicas::All`]).
    fn to_column(self) -> Option<i64> {
        match self {
            Self::Count(n) => Some(i64::from(n)),
            Self::All => None,
        }
    }

    /// Reads the stored column.
    fn from_column(value: Option<i64>) -> Self {
        match value {
            Some(n) => Self::Count(u32::try_from(n).unwrap_or(0)),
            None => Self::All,
        }
    }
}

/// What an operator asks for: how many nodes, and which may qualify.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlacementSpec {
    /// Nodes that should run the instance.
    pub replicas: Replicas,
    /// Label constraints every chosen node must satisfy.
    pub constraints: Vec<String>,
}

impl PlacementSpec {
    /// Validated copy with constraints trimmed, sorted, and deduplicated.
    ///
    /// # Errors
    ///
    /// Returns an error for a replica count over [`MAX_REPLICAS`], too many
    /// constraints, or a constraint outside the label grammar.
    pub fn normalized(&self) -> Result<Self> {
        if matches!(self.replicas, Replicas::Count(n) if n > MAX_REPLICAS) {
            return Err(invalid(&format!("replicas must be at most {MAX_REPLICAS}")));
        }
        let constraints = normalize(&self.constraints);
        if constraints.len() > MAX_PLACEMENT_CONSTRAINTS {
            return Err(invalid(&format!(
                "at most {MAX_PLACEMENT_CONSTRAINTS} placement constraints"
            )));
        }
        for constraint in &constraints {
            validate_constraint(constraint)?;
        }
        Ok(Self {
            replicas: self.replicas,
            constraints,
        })
    }
}

/// A stored placement: the operator's intent plus the last pass's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPlacement {
    /// Placed instance.
    pub plugin_instance_id: PluginInstanceId,
    /// Requested replicas and constraints.
    pub spec: PlacementSpec,
    /// Compare-and-swap revision of the intent.
    pub revision: i64,
    /// RFC 3339 time of the last intent write.
    pub updated_at: String,
    /// Audit identity of the last intent writer.
    pub updated_by: String,
    /// Replicas the last scheduler pass placed.
    pub placed: u32,
    /// Why the last pass placed fewer than requested; empty when satisfied.
    pub detail: String,
    /// RFC 3339 time the scheduler last changed the status; empty before
    /// the first pass.
    pub scheduled_at: String,
}

/// Outcome of [`put_plugin_placement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementWrite {
    /// The intent is stored at the returned revision.
    Applied(PluginPlacement),
    /// `expected_revision` was not current (0 when no placement exists).
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
}

/// Error for a value outside the placement grammar.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid configuration: {detail}"))
}

/// Only an operator writes placement intent and node labels.
fn authorize(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } => Ok(()),
        ConfigActor::Bootstrap
        | ConfigActor::Scheduler
        | ConfigActor::Administrator { .. }
        | ConfigActor::Member { .. }
        | ConfigActor::NodeOwner(_)
        | ConfigActor::ClusterOwner(_) => Err(LibraryError::Forbidden(
            "unauthorized configuration write: operator authority is required".into(),
        )),
    }
}

/// Trimmed, non-empty, sorted, deduplicated.
fn normalize(values: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = values
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    set.into_iter().collect()
}

/// True when every byte is a lowercase letter, digit, or one of `._/-`.
fn label_part_ok(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'/' | b'-')
        })
}

/// Checks the label grammar: `name` or `name=value`, lowercase letters,
/// digits, and `._/-`, the name starting with a letter or digit, at most 63
/// bytes.
///
/// # Errors
///
/// Returns an error when `label` does not fit the grammar.
pub fn validate_label(label: &str) -> Result<()> {
    let (name, value) = match label.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (label, None),
    };
    let starts_ok = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if label.len() > MAX_LABEL_BYTES
        || !starts_ok
        || !label_part_ok(name)
        || value.is_some_and(|v| !label_part_ok(v))
    {
        return Err(invalid(&format!(
            "label `{label}` must be `name` or `name=value` using lowercase letters, digits, \
             and `._/-` (at most {MAX_LABEL_BYTES} bytes)"
        )));
    }
    Ok(())
}

/// Checks an operator label: the grammar of [`validate_label`], and not one
/// of the implicit names every node already carries.
///
/// # Errors
///
/// Returns an error for a malformed or reserved label.
pub fn validate_node_label(label: &str) -> Result<()> {
    validate_label(label)?;
    let name = label.split_once('=').map_or(label, |(name, _)| name);
    if name == NODE_LABEL || name == TARGET_LABEL {
        return Err(invalid(&format!(
            "label name `{name}` is implicit on every node and cannot be assigned"
        )));
    }
    Ok(())
}

/// Checks a constraint: a label, optionally prefixed with `!` to exclude it.
///
/// # Errors
///
/// Returns an error when the label part does not fit the grammar.
pub fn validate_constraint(constraint: &str) -> Result<()> {
    validate_label(constraint.strip_prefix('!').unwrap_or(constraint))
}

/// A node's operator labels plus `node=<id>` and `target=<target>`.
#[must_use]
pub fn effective_node_labels(node: &NodeRecord) -> BTreeSet<String> {
    let mut labels: BTreeSet<String> = node.labels.iter().cloned().collect();
    labels.insert(format!(
        "{NODE_LABEL}={}",
        node.node_id.to_ascii_lowercase()
    ));
    if !node.target.is_empty() {
        labels.insert(format!("{TARGET_LABEL}={}", node.target));
    }
    labels
}

/// True when `labels` satisfy every constraint.
#[must_use]
pub fn labels_satisfy(labels: &BTreeSet<String>, constraints: &[String]) -> bool {
    constraints
        .iter()
        .all(|constraint| match constraint.strip_prefix('!') {
            Some(excluded) => !labels.contains(excluded),
            None => labels.contains(constraint.as_str()),
        })
}

/// Maps a stored row.
fn placement_from_model(row: plugin_placements::Model) -> Result<PluginPlacement> {
    let constraints: Vec<String> = serde_json::from_str(&row.constraints_json)
        .map_err(|err| invalid(&format!("stored placement constraints: {err}")))?;
    Ok(PluginPlacement {
        plugin_instance_id: PluginInstanceId::parse(&row.plugin_instance_id)?,
        spec: PlacementSpec {
            replicas: Replicas::from_column(row.replicas),
            constraints,
        },
        revision: row.revision,
        updated_at: row.updated_at,
        updated_by: row.updated_by,
        placed: u32::try_from(row.placed).unwrap_or(0),
        detail: row.detail,
        scheduled_at: row.scheduled_at,
    })
}

/// The placement of `instance_id`, if it has one.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_plugin_placement(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<Option<PluginPlacement>> {
    plugin_placements::Entity::find_by_id(instance_id.as_str().to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(placement_from_model)
        .transpose()
}

/// Every placement, by instance id.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_plugin_placements(store: &LibraryStore) -> Result<Vec<PluginPlacement>> {
    plugin_placements::Entity::find()
        .order_by_asc(plugin_placements::Column::PluginInstanceId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(placement_from_model)
        .collect()
}

/// Writes placement intent for an instance.
///
/// `expected_revision` is `None` (or `Some(0)`) to create a placement and the
/// current revision to replace one. Every node is woken so the scheduler,
/// wherever it runs, applies the change.
///
/// # Errors
///
/// Returns an error when the actor is not an operator, the spec is invalid,
/// the instance is missing, or the write fails.
pub async fn put_plugin_placement(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    spec: &PlacementSpec,
    expected_revision: Option<i64>,
) -> Result<PlacementWrite> {
    authorize(actor)?;
    let spec = spec.normalized()?;
    if load_plugin_instance(store, instance_id).await?.is_none() {
        return Err(LibraryError::NotFound(format!(
            "plugin instance {instance_id}"
        )));
    }
    let constraints_json = serde_json::to_string(&spec.constraints)
        .map_err(|err| invalid(&format!("placement constraints: {err}")))?;
    let now = Utc::now().to_rfc3339();
    let current = load_plugin_placement(store, instance_id).await?;
    let expected = expected_revision.unwrap_or(0);
    let current_revision = current.as_ref().map_or(0, |p| p.revision);
    if expected != current_revision {
        return Ok(PlacementWrite::Conflict { current_revision });
    }
    let replicas = spec.replicas.to_column();
    if current.is_none() {
        let model = plugin_placements::ActiveModel {
            plugin_instance_id: Set(instance_id.as_str().to_string()),
            replicas: Set(replicas),
            constraints_json: Set(constraints_json),
            revision: Set(1),
            updated_at: Set(now),
            updated_by: Set(actor.audit_id().to_string()),
            placed: Set(0),
            detail: Set(String::new()),
            scheduled_at: Set(String::new()),
        };
        if let Err(err) = plugin_placements::Entity::insert(model)
            .exec(store.db())
            .await
        {
            return match load_plugin_placement(store, instance_id).await? {
                Some(raced) => Ok(PlacementWrite::Conflict {
                    current_revision: raced.revision,
                }),
                None => Err(LibraryError::Orm(err)),
            };
        }
    } else {
        let res = plugin_placements::Entity::update_many()
            .col_expr(plugin_placements::Column::Replicas, Expr::value(replicas))
            .col_expr(
                plugin_placements::Column::ConstraintsJson,
                Expr::value(constraints_json),
            )
            .col_expr(
                plugin_placements::Column::Revision,
                Expr::value(expected + 1),
            )
            .col_expr(plugin_placements::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                plugin_placements::Column::UpdatedBy,
                Expr::value(actor.audit_id().to_string()),
            )
            .filter(plugin_placements::Column::PluginInstanceId.eq(instance_id.as_str()))
            .filter(plugin_placements::Column::Revision.eq(expected))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected != 1 {
            let current_revision = load_plugin_placement(store, instance_id)
                .await?
                .map_or(0, |p| p.revision);
            return Ok(PlacementWrite::Conflict { current_revision });
        }
    }
    let stored = load_plugin_placement(store, instance_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("placement {instance_id}")))?;
    notify_all_nodes(
        store,
        NodeMessage::PlacementChanged {
            subject: format!("instance:{instance_id}"),
            revision: stored.revision,
        },
    )
    .await;
    Ok(PlacementWrite::Applied(stored))
}

/// Rounds a hand-back or undeploy re-reads its rows and retries the ones
/// whose compare-and-swap lost to a concurrent writer (an in-flight
/// scheduler pass, another operator).
const SETTLE_ROUNDS: usize = 5;

/// Moves every deployment of `instance_id` for which `next` returns a state
/// to that state, re-reading and retrying rows that lose a compare-and-swap
/// for up to [`SETTLE_ROUNDS`] rounds. Returns how many rows changed.
///
/// # Errors
///
/// Returns [`LibraryError::Conflict`] when rows still lose after the last
/// round, and the read or write error when one fails.
async fn settle_deployments(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    what: &str,
    next: impl Fn(DesiredState) -> Option<DesiredState>,
) -> Result<usize> {
    let mut changed = 0;
    for _ in 0..SETTLE_ROUNDS {
        let mut lost = 0;
        for row in list_deployments_for_instance(store, instance_id).await? {
            let Some(target) = next(row.desired) else {
                continue;
            };
            let operation_id = format!("{what}-{}-{}", row.deployment_id, row.revision);
            match replace_plugin_deployment(
                store,
                actor,
                &row.deployment_id,
                row.revision,
                target,
                &operation_id,
            )
            .await?
            {
                DeploymentReplace::Conflict { .. } => lost += 1,
                DeploymentReplace::Applied(_) | DeploymentReplace::Replayed { .. } => changed += 1,
            }
        }
        if lost == 0 {
            return Ok(changed);
        }
    }
    Err(LibraryError::Conflict(format!(
        "deployments of instance {instance_id} kept changing during {what}; retry"
    )))
}

/// Deletes an instance's placement and hands its deployments back to the
/// operator: each `placed` row becomes `present` and each `unplaced` row
/// `absent`, so every node keeps doing what it does now. Returns false when
/// there was no placement.
///
/// The placement row goes first, so a scheduler pass that starts now finds
/// no placement and leaves these rows alone. A pass already in flight skips
/// an instance whose placement is gone, and a row it still manages to write
/// is handed back by the next pass (see [`schedule_placements`]).
///
/// # Errors
///
/// Returns an error when the actor is not an operator, a write fails, or a
/// row keeps losing its compare-and-swap ([`LibraryError::Conflict`]).
pub async fn delete_plugin_placement(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
) -> Result<bool> {
    authorize(actor)?;
    let res = plugin_placements::Entity::delete_by_id(instance_id.as_str().to_string())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    settle_deployments(store, actor, instance_id, "hand-back", |desired| {
        desired.is_scheduled().then(|| desired.handed_back())
    })
    .await?;
    if res.rows_affected > 0 {
        notify_all_nodes(
            store,
            NodeMessage::PlacementChanged {
                subject: format!("instance:{instance_id}"),
                revision: 0,
            },
        )
        .await;
    }
    Ok(res.rows_affected > 0)
}

/// Stops `instance_id` everywhere: drops its placement (handing its rows
/// back) and sets every running deployment `absent`. Returns how many
/// deployments were stopped. Uninstall runs this for each instance before
/// it clears the active version.
///
/// # Errors
///
/// Returns an error when the actor is not an operator, a write fails, or a
/// row keeps losing its compare-and-swap ([`LibraryError::Conflict`]); the
/// caller must then not treat the instance as stopped.
pub async fn undeploy_instance(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
) -> Result<usize> {
    delete_plugin_placement(store, actor, instance_id).await?;
    settle_deployments(store, actor, instance_id, "undeploy", |desired| {
        desired.runs().then_some(DesiredState::Absent)
    })
    .await
}

/// Fails when the scheduler owns `instance_id`'s deployments, so a
/// hand-written enable or disable is not silently undone on its next pass.
///
/// # Errors
///
/// Returns a conflict naming the placement when one exists.
pub async fn ensure_hand_managed(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<()> {
    if let Some(placement) = load_plugin_placement(store, instance_id).await? {
        return Err(LibraryError::Conflict(format!(
            "plugin instance {instance_id} is placed by the scheduler ({} replica(s){}); \
             change it with `bookclerk plugins deploy`, or drop the placement with \
             `bookclerk plugins deploy --manual` first",
            placement.spec.replicas,
            if placement.spec.constraints.is_empty() {
                String::new()
            } else {
                format!(" on {}", placement.spec.constraints.join(", "))
            }
        )));
    }
    Ok(())
}

/// Replaces a node's operator labels and wakes the scheduler.
///
/// # Errors
///
/// Returns an error when the actor is not an operator, a label is invalid or
/// reserved, there are too many labels, or the node is unknown.
pub async fn set_node_labels(
    store: &LibraryStore,
    actor: &ConfigActor,
    node_id: &str,
    labels: &[String],
) -> Result<NodeRecord> {
    authorize(actor)?;
    let labels = normalize(labels);
    if labels.len() > MAX_NODE_LABELS {
        return Err(invalid(&format!("at most {MAX_NODE_LABELS} node labels")));
    }
    for label in &labels {
        validate_node_label(label)?;
    }
    let json =
        serde_json::to_string(&labels).map_err(|err| invalid(&format!("node labels: {err}")))?;
    let res = nodes::Entity::update_many()
        .col_expr(nodes::Column::LabelsJson, Expr::value(json))
        .filter(nodes::Column::NodeId.eq(node_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if res.rows_affected == 0 {
        return Err(LibraryError::NotFound(format!("node {node_id}")));
    }
    notify_all_nodes(
        store,
        NodeMessage::PlacementChanged {
            subject: format!("node:{node_id}"),
            revision: 0,
        },
    )
    .await;
    load_node(store, node_id).await
}

/// Nodes whose daemon is live: its last heartbeat in `node_status` is
/// younger than three heartbeat intervals.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn live_node_ids(store: &LibraryStore) -> Result<BTreeSet<String>> {
    let now_ms = Utc::now().timestamp_millis();
    Ok(super::node_status::list_node_statuses(store)
        .await?
        .into_iter()
        .filter(|status| status.is_live(now_ms))
        .map(|status| status.node_id)
        .collect())
}

/// A held cluster lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterLease {
    /// Lease name.
    pub name: String,
    /// Holder (a node id).
    pub holder: String,
    /// Fencing token; a new holder always gets a larger one.
    pub token: i64,
    /// Expiry in Unix milliseconds.
    pub expires_at_ms: i64,
}

/// Acquires or renews `name` for `holder` for `ttl`.
///
/// Returns `None` when another holder's lease has not expired, or when a
/// concurrent acquire won. Renewing keeps the token; taking over an expired
/// lease bumps it.
///
/// # Errors
///
/// Returns an error when the read or write fails.
pub async fn acquire_lease(
    store: &LibraryStore,
    name: &str,
    holder: &str,
    ttl: Duration,
) -> Result<Option<ClusterLease>> {
    let now_ms = Utc::now().timestamp_millis();
    let expires_at_ms = now_ms.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
    let renewed_at = Utc::now().to_rfc3339();
    let lease = |token| ClusterLease {
        name: name.to_string(),
        holder: holder.to_string(),
        token,
        expires_at_ms,
    };
    let Some(row) = cluster_leases::Entity::find_by_id(name.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        let model = cluster_leases::ActiveModel {
            name: Set(name.to_string()),
            holder: Set(holder.to_string()),
            token: Set(1),
            expires_at_ms: Set(expires_at_ms),
            renewed_at: Set(renewed_at),
        };
        return match cluster_leases::Entity::insert(model).exec(store.db()).await {
            Ok(_) => Ok(Some(lease(1))),
            // A concurrent first acquire inserted the row; this one lost.
            Err(err) => match load_lease(store, name).await? {
                Some(_) => Ok(None),
                None => Err(LibraryError::Orm(err)),
            },
        };
    };
    let token = if row.holder == holder {
        row.token
    } else if row.expires_at_ms <= now_ms {
        row.token + 1
    } else {
        return Ok(None);
    };
    let res = cluster_leases::Entity::update_many()
        .col_expr(
            cluster_leases::Column::Holder,
            Expr::value(holder.to_string()),
        )
        .col_expr(cluster_leases::Column::Token, Expr::value(token))
        .col_expr(
            cluster_leases::Column::ExpiresAtMs,
            Expr::value(expires_at_ms),
        )
        .col_expr(cluster_leases::Column::RenewedAt, Expr::value(renewed_at))
        .filter(cluster_leases::Column::Name.eq(name))
        .filter(cluster_leases::Column::Holder.eq(row.holder))
        .filter(cluster_leases::Column::Token.eq(row.token))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok((res.rows_affected == 1).then(|| lease(token)))
}

/// Gives up a held lease so another holder may take it at once.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn release_lease(store: &LibraryStore, lease: &ClusterLease) -> Result<()> {
    cluster_leases::Entity::update_many()
        .col_expr(cluster_leases::Column::ExpiresAtMs, Expr::value(0_i64))
        .filter(cluster_leases::Column::Name.eq(lease.name.as_str()))
        .filter(cluster_leases::Column::Holder.eq(lease.holder.as_str()))
        .filter(cluster_leases::Column::Token.eq(lease.token))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// The current row of lease `name`, held or expired.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_lease(store: &LibraryStore, name: &str) -> Result<Option<ClusterLease>> {
    Ok(cluster_leases::Entity::find_by_id(name.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(|row| ClusterLease {
            name: row.name,
            holder: row.holder,
            token: row.token,
            expires_at_ms: row.expires_at_ms,
        }))
}

/// One node as the planner sees it: identity, liveness, and the budget
/// headroom its last heartbeat reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCandidate {
    /// Node id.
    pub node_id: String,
    /// Effective labels ([`effective_node_labels`]).
    pub labels: BTreeSet<String>,
    /// Artifact target; empty when unknown.
    pub target: String,
    /// True when the node's daemon heartbeated recently.
    pub live: bool,
    /// False when an operator drained the node.
    pub schedulable: bool,
    /// CPU still uncommitted under the node's budget, in millicores
    /// (measured use counts when it exceeds the reservations).
    pub cpu_headroom_millicores: i64,
    /// Memory still uncommitted under the budget and free on the machine;
    /// empty when the node could not detect its memory.
    pub memory_headroom_bytes: Option<i64>,
    /// Recent online fraction in parts per million.
    pub availability_ppm: i64,
}

impl NodeCandidate {
    /// A node as of its last heartbeat (`None`: its daemon never ran, so it
    /// is not live).
    #[must_use]
    pub fn from_status(node: &NodeRecord, status: Option<&NodeStatus>, now_ms: i64) -> Self {
        Self {
            node_id: node.node_id.clone(),
            labels: effective_node_labels(node),
            target: node.target.clone(),
            live: status.is_some_and(|s| s.is_live(now_ms)),
            schedulable: status.is_none_or(|s| s.schedulable),
            cpu_headroom_millicores: status.map_or(0, NodeStatus::cpu_headroom_millicores),
            memory_headroom_bytes: status.and_then(NodeStatus::memory_headroom_bytes),
            availability_ppm: status.map_or(0, |s| s.availability_ppm),
        }
    }

    /// True when one more guest needing `demand` fits the memory budget.
    /// Memory is the hard limit (a guest over it is killed); CPU only ranks.
    #[must_use]
    pub fn fits(&self, demand: GuestReservation) -> bool {
        self.memory_headroom_bytes
            .is_none_or(|headroom| headroom >= demand.memory_bytes)
    }

    /// True when the node has committed more memory than its budget, or
    /// more than the machine has free.
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.memory_headroom_bytes
            .is_some_and(|headroom| headroom < 0)
    }

    /// How many more guests like `demand` the node has room for, in
    /// thousandths, weighted by availability (an always-on node counts in
    /// full, a node that is never on counts half). Larger is better.
    #[must_use]
    pub fn score(&self, demand: GuestReservation) -> i64 {
        let cpu_room =
            self.cpu_headroom_millicores.max(0).saturating_mul(1000) / demand.cpu_millicores.max(1);
        let room = match self.memory_headroom_bytes {
            Some(memory) => {
                cpu_room.min(memory.max(0).saturating_mul(1000) / demand.memory_bytes.max(1))
            }
            None => cpu_room,
        };
        room.saturating_mul(500_000 + self.availability_ppm.clamp(0, 1_000_000) / 2) / 1_000_000
    }

    /// Commits `demand` on this node for the rest of a pass.
    pub fn take(&mut self, demand: GuestReservation) {
        self.cpu_headroom_millicores = self
            .cpu_headroom_millicores
            .saturating_sub(demand.cpu_millicores);
        self.memory_headroom_bytes = self
            .memory_headroom_bytes
            .map(|m| m.saturating_sub(demand.memory_bytes));
    }
}

/// What the planner decided for one instance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlacementPlan {
    /// Nodes that should gain a replica.
    pub add: Vec<String>,
    /// Nodes whose replica should be removed.
    pub remove: Vec<String>,
    /// Replicas placed after the plan applies.
    pub placed: u32,
    /// Why fewer than requested are placed; empty when satisfied.
    pub detail: String,
}

/// Plans one instance.
///
/// `version_targets` is the active release's artifact targets (`None` for a
/// plugin with no release), `demand` what one guest of the instance needs,
/// and `present` the nodes that already run it.
///
/// A node qualifies when it is live, schedulable, satisfies the
/// constraints, and has an artifact for its target. Replicas stay where
/// they run while their node qualifies. New replicas go to the qualifying
/// nodes with the most room for this demand, and only to nodes where its
/// memory fits the budget. A scale-down drops the replicas on the nodes
/// with the least room first. When a replica's node is over its memory
/// budget and another qualifying node has room, the replica moves there
/// (one move per pass, so the cluster does not thrash).
#[must_use]
pub fn plan_placement(
    spec: &PlacementSpec,
    version_targets: Option<&BTreeSet<String>>,
    demand: GuestReservation,
    nodes: &[NodeCandidate],
    present: &BTreeSet<String>,
) -> PlacementPlan {
    // Same rule as `PluginVersion::artifact_for`: an exact target, or a
    // target-independent (`any`) artifact. A node that has not reported its
    // target is not excluded; its reconciler reports a missing artifact.
    let target_ok = |node: &NodeCandidate| match version_targets {
        Some(targets) => {
            node.target.is_empty() || targets.contains(&node.target) || targets.contains("any")
        }
        None => true,
    };
    let qualifies = |node: &NodeCandidate| {
        node.live
            && node.schedulable
            && labels_satisfy(&node.labels, &spec.constraints)
            && target_ok(node)
    };
    let by_id: BTreeMap<&str, &NodeCandidate> =
        nodes.iter().map(|h| (h.node_id.as_str(), h)).collect();
    let mut plan = PlacementPlan::default();

    // Replicas stay while their node qualifies.
    let mut keep: Vec<&NodeCandidate> = Vec::new();
    for node_id in present {
        match by_id.get(node_id.as_str()) {
            Some(node) if qualifies(node) => keep.push(node),
            _ => plan.remove.push(node_id.clone()),
        }
    }
    // Most room first: the end of the list is what a scale-down drops.
    keep.sort_by(|a, b| {
        b.score(demand)
            .cmp(&a.score(demand))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    let mut candidates: Vec<&NodeCandidate> = nodes
        .iter()
        .filter(|h| !present.contains(&h.node_id) && qualifies(h) && h.fits(demand))
        .collect();
    candidates.sort_by(|a, b| {
        b.score(demand)
            .cmp(&a.score(demand))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });

    let qualifying = nodes.iter().filter(|h| qualifies(h)).count();
    let lacking_room = nodes
        .iter()
        .filter(|h| !present.contains(&h.node_id) && qualifies(h) && !h.fits(demand))
        .count();
    match spec.replicas {
        Replicas::Count(n) => {
            let want = usize::try_from(n).unwrap_or(usize::MAX);
            if keep.len() > want {
                for node in keep.split_off(want) {
                    plan.remove.push(node.node_id.clone());
                }
            }
            let need = want.saturating_sub(keep.len());
            let mut fresh = candidates.into_iter();
            plan.add = fresh
                .by_ref()
                .take(need)
                .map(|h| h.node_id.clone())
                .collect();
            // Rebalance: move one replica off an over-budget node when a
            // node with room is left over.
            if plan.add.len() == need {
                if let (Some(pos), Some(better)) =
                    (keep.iter().rposition(|h| h.over_budget()), fresh.next())
                {
                    let from = keep.remove(pos);
                    plan.remove.push(from.node_id.clone());
                    plan.add.push(better.node_id.clone());
                }
            }
            let placed = keep.len() + plan.add.len();
            plan.placed = u32::try_from(placed).unwrap_or(u32::MAX);
            if placed < want {
                plan.detail = shortfall(
                    &format!("{placed} of {n} replica(s) placed"),
                    qualifying,
                    lacking_room,
                    spec,
                    demand,
                );
            }
        }
        Replicas::All => {
            plan.add = candidates.iter().map(|h| h.node_id.clone()).collect();
            // Every qualifying node runs it; shed one replica from an
            // over-budget node while another node still runs it.
            if keep.len() + plan.add.len() > 1 {
                if let Some(pos) = keep.iter().rposition(|h| h.over_budget()) {
                    let from = keep.remove(pos);
                    plan.remove.push(from.node_id.clone());
                }
            }
            let placed = keep.len() + plan.add.len();
            plan.placed = u32::try_from(placed).unwrap_or(u32::MAX);
            if placed == 0 || lacking_room > 0 {
                plan.detail = shortfall(
                    &format!("{placed} replica(s) placed"),
                    qualifying,
                    lacking_room,
                    spec,
                    demand,
                );
            }
        }
    }
    plan.remove.sort();
    plan.remove.dedup();
    plan
}

/// Why a placement is short: how many nodes qualify and how many lack room.
fn shortfall(
    head: &str,
    qualifying: usize,
    lacking_room: usize,
    spec: &PlacementSpec,
    demand: GuestReservation,
) -> String {
    let rule = if spec.constraints.is_empty() {
        "are live and schedulable".to_string()
    } else {
        format!(
            "are live, schedulable, and match {}",
            spec.constraints.join(", ")
        )
    };
    let mut text = format!("{head}: {qualifying} node(s) {rule}");
    if lacking_room > 0 {
        text.push_str(&format!(
            "; {lacking_room} lack memory budget for {} MiB",
            demand.memory_bytes / (1024 * 1024)
        ));
    }
    text
}

/// What one more guest of a placed instance needs: the largest ceiling any
/// node recorded for it, else its release grant's memory and CPU, else the
/// grant defaults.
async fn placement_demand(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    release: Option<&super::versions::PluginVersion>,
) -> Result<GuestReservation> {
    if let Some(observed) = super::node_status::instance_demand(store, instance_id).await? {
        return Ok(observed);
    }
    let mut demand = GuestReservation::default_guest();
    if let Some(grant) = release
        .filter(|r| !r.grant_json.trim().is_empty())
        .and_then(|r| serde_json::from_str::<serde_json::Value>(&r.grant_json).ok())
    {
        if let Some(mib) = grant.get("memoryMib").and_then(serde_json::Value::as_i64) {
            demand.memory_bytes = mib.max(1).saturating_mul(1024 * 1024);
        }
        if let Some(percent) = grant
            .get("cpuRatePercent")
            .and_then(serde_json::Value::as_i64)
        {
            demand.cpu_millicores = percent.max(1).saturating_mul(10);
        }
    }
    Ok(demand)
}

/// What one scheduler pass changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduleReport {
    /// Placements considered.
    pub placements: usize,
    /// Deployments set `placed`.
    pub added: usize,
    /// Deployments set `unplaced`.
    pub removed: usize,
    /// Deployment writes that lost a compare-and-swap (retried next pass).
    pub conflicts: usize,
    /// Scheduler-owned rows of instances with no placement, handed back to
    /// the operator (a pass that raced a `--manual` or uninstall).
    pub handed_back: usize,
}

/// Sets `node_id`'s deployment of `instance_id` to `placed` (`true`) or
/// `unplaced`, creating the row when missing. Returns false on a
/// compare-and-swap loss.
async fn write_deployment(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    node_id: &str,
    existing: Option<&PluginDeployment>,
    placed: bool,
) -> Result<bool> {
    let actor = ConfigActor::Scheduler;
    let desired = DesiredState::scheduled(placed);
    let row = match existing {
        Some(row) => row.clone(),
        // `ensure` returns a row a concurrent writer created first; that row
        // may carry another state, so fall through to replace it.
        None => ensure_plugin_deployment(store, &actor, instance_id, node_id, desired).await?,
    };
    if row.desired == desired {
        return Ok(true);
    }
    let operation_id = format!("scheduler-{}-{}-{desired}", row.deployment_id, row.revision);
    Ok(!matches!(
        replace_plugin_deployment(
            store,
            &actor,
            &row.deployment_id,
            row.revision,
            desired,
            &operation_id,
        )
        .await?,
        DeploymentReplace::Conflict { .. }
    ))
}

/// One scheduler pass: plans every placement against the nodes' last
/// heartbeats and writes the deployments the plans call for. Call it only
/// while holding the [`SCHEDULER_LEASE`].
///
/// Headroom is tracked across the pass: a replica placed for one instance
/// is committed against its node before the next instance is planned.
///
/// # Errors
///
/// Returns an error when a read fails or a deployment write is refused.
pub async fn schedule_placements(store: &LibraryStore) -> Result<ScheduleReport> {
    let placements = list_plugin_placements(store).await?;
    let mut report = ScheduleReport {
        placements: placements.len(),
        ..ScheduleReport::default()
    };
    // No early return when nothing is placed: the hand-back of orphaned
    // scheduler rows below still has to run.
    let now_ms = Utc::now().timestamp_millis();
    let statuses: BTreeMap<String, NodeStatus> = super::node_status::list_node_statuses(store)
        .await?
        .into_iter()
        .map(|status| (status.node_id.clone(), status))
        .collect();
    let mut candidates: BTreeMap<String, NodeCandidate> = list_nodes(store)
        .await?
        .iter()
        .map(|node| {
            (
                node.node_id.clone(),
                NodeCandidate::from_status(node, statuses.get(&node.node_id), now_ms),
            )
        })
        .collect();
    let mut by_instance: BTreeMap<String, Vec<PluginDeployment>> = BTreeMap::new();
    for deployment in list_all_deployments(store).await? {
        by_instance
            .entry(deployment.plugin_instance_id.as_str().to_string())
            .or_default()
            .push(deployment);
    }
    for placement in placements {
        let instance_id = &placement.plugin_instance_id;
        let rows = by_instance.remove(instance_id.as_str()).unwrap_or_default();
        let Some(instance) = load_plugin_instance(store, instance_id).await? else {
            continue;
        };
        let release = load_active_version(store, &instance.plugin_key).await?;
        let version_targets = release.as_ref().map(|release| {
            release
                .artifacts
                .iter()
                .map(|a| a.target.clone())
                .collect::<BTreeSet<_>>()
        });
        let demand = placement_demand(store, instance_id, release.as_ref()).await?;
        let present: BTreeSet<String> = rows
            .iter()
            .filter(|d| d.desired.runs())
            .map(|d| d.node_id.clone())
            .collect();
        let nodes: Vec<NodeCandidate> = candidates.values().cloned().collect();
        let plan = plan_placement(
            &placement.spec,
            version_targets.as_ref(),
            demand,
            &nodes,
            &present,
        );
        // The placement may have been deleted (`--manual`, uninstall) or
        // edited since this pass listed it; writing now would recreate
        // scheduler rows the operator just took back. The next pass replans.
        match load_plugin_placement(store, instance_id).await? {
            Some(current) if current.revision == placement.revision => {}
            _ => continue,
        }
        let mut running = present.clone();
        let mut lost = 0;
        for node_id in &plan.remove {
            let row = rows.iter().find(|d| &d.node_id == node_id);
            // Headroom is not given back: the guest stays reserved until
            // its node stops it, so later instances in this pass must not
            // be packed onto it.
            if write_deployment(store, instance_id, node_id, row, false).await? {
                report.removed += 1;
                running.remove(node_id);
            } else {
                lost += 1;
            }
        }
        for node_id in &plan.add {
            let row = rows.iter().find(|d| &d.node_id == node_id);
            if write_deployment(store, instance_id, node_id, row, true).await? {
                report.added += 1;
                running.insert(node_id.clone());
                if let Some(node) = candidates.get_mut(node_id) {
                    node.take(demand);
                }
            } else {
                lost += 1;
            }
        }
        // A row the operator handed over still reads `present`/`absent`;
        // rewrite kept rows into the scheduler's own states.
        for row in &rows {
            if row.desired.is_scheduled()
                || plan.add.contains(&row.node_id)
                || plan.remove.contains(&row.node_id)
            {
                continue;
            }
            if write_deployment(
                store,
                instance_id,
                &row.node_id,
                Some(row),
                row.desired.runs(),
            )
            .await?
            {
                continue;
            }
            lost += 1;
        }
        report.conflicts += lost;
        // Report what the writes did, not what the plan wanted: a write that
        // lost its compare-and-swap leaves that node as it was.
        let placed = u32::try_from(running.len()).unwrap_or(u32::MAX);
        let detail = if lost == 0 {
            plan.detail
        } else {
            let pending = format!("{lost} deployment write(s) lost a race; retrying next pass");
            if plan.detail.is_empty() {
                pending
            } else {
                format!("{}; {pending}", plan.detail)
            }
        };
        if placed != placement.placed || detail != placement.detail {
            plugin_placements::Entity::update_many()
                .col_expr(
                    plugin_placements::Column::Placed,
                    Expr::value(i64::from(placed)),
                )
                .col_expr(plugin_placements::Column::Detail, Expr::value(detail))
                .col_expr(
                    plugin_placements::Column::ScheduledAt,
                    Expr::value(Utc::now().to_rfc3339()),
                )
                .filter(plugin_placements::Column::PluginInstanceId.eq(instance_id.as_str()))
                .exec(store.db())
                .await
                .map_err(LibraryError::Orm)?;
        }
    }
    // Scheduler-owned rows whose placement is gone: a pass raced the
    // placement's deletion. Hand them back as the deletion would have.
    for rows in by_instance.into_values() {
        let Some(first) = rows.first() else {
            continue;
        };
        if !rows.iter().any(|row| row.desired.is_scheduled())
            || load_plugin_placement(store, &first.plugin_instance_id)
                .await?
                .is_some()
        {
            continue;
        }
        for row in rows.iter().filter(|row| row.desired.is_scheduled()) {
            let operation_id = format!("orphan-hand-back-{}-{}", row.deployment_id, row.revision);
            match replace_plugin_deployment(
                store,
                &ConfigActor::Scheduler,
                &row.deployment_id,
                row.revision,
                row.desired.handed_back(),
                &operation_id,
            )
            .await?
            {
                DeploymentReplace::Conflict { .. } => report.conflicts += 1,
                DeploymentReplace::Applied(_) | DeploymentReplace::Replayed { .. } => {
                    report.handed_back += 1;
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    /// A live, schedulable node with `cores` of CPU headroom and `mem_gib`
    /// of memory headroom.
    fn node(id: &str, labels: &[&str], cores: i64, mem_gib: i64) -> NodeCandidate {
        let mut set: BTreeSet<String> = labels.iter().map(|l| (*l).to_string()).collect();
        set.insert(format!("node={id}"));
        NodeCandidate {
            node_id: id.into(),
            labels: set,
            target: "linux-x64-gnu".into(),
            live: true,
            schedulable: true,
            cpu_headroom_millicores: cores * 1000,
            memory_headroom_bytes: Some(mem_gib * GIB),
            availability_ppm: 1_000_000,
        }
    }

    fn demand() -> GuestReservation {
        GuestReservation {
            memory_bytes: GIB / 2,
            cpu_millicores: 800,
        }
    }

    fn spec(replicas: Replicas, constraints: &[&str]) -> PlacementSpec {
        PlacementSpec {
            replicas,
            constraints: constraints.iter().map(|c| (*c).to_string()).collect(),
        }
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|i| (*i).to_string()).collect()
    }

    #[test]
    fn new_replicas_go_to_the_nodes_with_the_most_room() {
        // A small always-on VPS and a big desktop: the desktop has room for
        // far more guests, so it wins; the VPS is next.
        let vps = node("vps", &["nas"], 1, 1);
        let desktop = node("desk", &["nas"], 12, 24);
        let other = node("other", &[], 16, 64);
        let plan = plan_placement(
            &spec(Replicas::Count(2), &["nas"]),
            None,
            demand(),
            &[vps, desktop, other],
            &BTreeSet::new(),
        );
        assert_eq!(plan.add, vec!["desk", "vps"]);
        assert_eq!(plan.placed, 2);
        assert!(plan.detail.is_empty());
    }

    #[test]
    fn availability_discounts_room_and_memory_budget_is_a_hard_limit() {
        let mut flaky = node("flaky", &[], 8, 16);
        flaky.availability_ppm = 0; // never on: room counts half
        let steady = node("steady", &[], 5, 16);
        let full = node("full", &[], 32, 0); // no memory budget left
        let plan = plan_placement(
            &spec(Replicas::Count(1), &[]),
            None,
            demand(),
            &[flaky.clone(), steady.clone(), full],
            &BTreeSet::new(),
        );
        assert_eq!(plan.add, vec!["steady"], "5 steady cores beat 8 flaky ones");
        assert!(flaky.score(demand()) < steady.score(demand()));
        let plan = plan_placement(
            &spec(Replicas::Count(3), &[]),
            None,
            demand(),
            &[node("full", &[], 32, 0)],
            &BTreeSet::new(),
        );
        assert!(plan.add.is_empty());
        assert!(
            plan.detail.contains("1 lack memory budget for 512 MiB"),
            "{}",
            plan.detail
        );
    }

    #[test]
    fn dead_drained_or_unlabelled_nodes_lose_their_replicas() {
        let mut dead = node("dead", &["nas"], 4, 8);
        dead.live = false;
        let mut drained = node("drained", &["nas"], 4, 8);
        drained.schedulable = false;
        let unlabelled = node("plain", &[], 4, 8);
        let fresh = node("fresh", &["nas"], 4, 8);
        let plan = plan_placement(
            &spec(Replicas::Count(1), &["nas"]),
            None,
            demand(),
            &[dead, drained, unlabelled, fresh],
            &set(&["dead", "drained", "plain"]),
        );
        assert_eq!(plan.remove, vec!["dead", "drained", "plain"]);
        assert_eq!(plan.add, vec!["fresh"]);
    }

    #[test]
    fn scale_down_drops_the_least_room_and_an_over_budget_replica_moves() {
        let roomy = node("roomy", &[], 8, 16);
        let tight = node("tight", &[], 1, 1);
        let plan = plan_placement(
            &spec(Replicas::Count(1), &[]),
            None,
            demand(),
            &[roomy.clone(), tight.clone()],
            &set(&["roomy", "tight"]),
        );
        assert_eq!(plan.remove, vec!["tight"]);
        assert!(plan.add.is_empty());

        let mut overloaded = node("over", &[], 0, 0);
        overloaded.memory_headroom_bytes = Some(-GIB);
        let plan = plan_placement(
            &spec(Replicas::Count(1), &[]),
            None,
            demand(),
            &[overloaded, roomy],
            &set(&["over"]),
        );
        assert_eq!(plan.remove, vec!["over"]);
        assert_eq!(plan.add, vec!["roomy"]);
        assert_eq!(plan.placed, 1);
    }

    #[test]
    fn all_runs_everywhere_with_room_and_targets_still_filter() {
        let mut mac = node("mac", &[], 8, 16);
        mac.target = "macos-arm64".into();
        let mut unknown = node("old", &[], 2, 2);
        unknown.target = String::new();
        let x86 = node("x86", &[], 4, 8);
        let tiny = node("tiny", &[], 4, 0);
        let targets = set(&["linux-x64-gnu"]);
        let plan = plan_placement(
            &spec(Replicas::All, &[]),
            Some(&targets),
            demand(),
            &[mac.clone(), unknown, x86, tiny],
            &BTreeSet::new(),
        );
        assert_eq!(plan.add, vec!["x86", "old"]);
        assert_eq!(plan.placed, 2);
        assert!(plan.detail.contains("lack memory"), "{}", plan.detail);
        // A workerd package ships one `any` artifact that runs everywhere.
        let any = plan_placement(
            &spec(Replicas::All, &[]),
            Some(&set(&["any"])),
            demand(),
            &[mac],
            &BTreeSet::new(),
        );
        assert_eq!(any.add, vec!["mac"]);
        let pinned = plan_placement(
            &spec(Replicas::Count(1), &["node=x86", "!slow"]),
            None,
            demand(),
            &[node("x86", &[], 1, 1), node("big", &[], 32, 64)],
            &BTreeSet::new(),
        );
        assert_eq!(pinned.add, vec!["x86"]);
    }

    #[test]
    fn replicas_parse_and_round_trip() {
        assert_eq!("all".parse::<Replicas>().unwrap(), Replicas::All);
        assert_eq!(" 3 ".parse::<Replicas>().unwrap(), Replicas::Count(3));
        assert!("many".parse::<Replicas>().is_err());
        assert_eq!(serde_json::to_string(&Replicas::All).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::from_str::<Replicas>("2").unwrap(),
            Replicas::Count(2)
        );
        assert_eq!(
            serde_json::from_str::<Replicas>("\"all\"").unwrap(),
            Replicas::All
        );
        assert_eq!(
            Replicas::from_column(Replicas::All.to_column()),
            Replicas::All
        );
    }

    #[test]
    fn label_grammar() {
        for ok in ["nas", "disk=ssd", "zone=eu-west/1", "a.b_c-d"] {
            validate_label(ok).unwrap();
            validate_node_label(ok).unwrap();
            validate_constraint(&format!("!{ok}")).unwrap();
        }
        for bad in [
            "",
            "NAS",
            "-x",
            "a=",
            "=b",
            "a b",
            "a=b=c=D",
            &"x".repeat(64),
        ] {
            assert!(validate_label(bad).is_err(), "{bad}");
        }
        assert!(validate_node_label("node=abc").is_err());
        assert!(validate_node_label("target").is_err());
        validate_constraint("node=abc").unwrap();
        assert!(spec(Replicas::Count(MAX_REPLICAS + 1), &[])
            .normalized()
            .is_err());
        let normalized = spec(Replicas::Count(1), &[" b ", "a", "b", ""])
            .normalized()
            .unwrap();
        assert_eq!(normalized.constraints, vec!["a", "b"]);
    }
}
