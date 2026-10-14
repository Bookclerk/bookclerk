//! Stable node identity file and database registration.
//!
//! The identity file is bootstrap material. Hostname text is never an input.
//! A process incarnation is memory-only and is written as a heartbeat observation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bookclerk_sandbox::{
    filesystem_free_bytes, logical_cpu_count, process_cgroup_dir, read_cgroup_sample,
};
use chrono::Utc;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::batch::{self, exec, int, int_opt, request, text};
use crate::entities::nodes;
use crate::error::{LibraryError, Result};
use crate::host_schema::current_schema_state;
use crate::migrations::{unreleased_checksum, SCHEMA_VERSION};
use crate::schema_state::SchemaState;
use crate::store::LibraryStore;

/// Bootstrap file under `$BOOKCLERK_FILES_DIR` that stores the stable node id.
pub const NODE_IDENTITY_FILE: &str = "node-identity.json";

/// Identity file schema. Unknown versions fail closed.
const IDENTITY_FILE_VERSION: u32 = 1;

/// Process-local incarnation. A restart mints a new value; the node id does not.
fn process_incarnation() -> &'static str {
    static INCARNATION: OnceLock<String> = OnceLock::new();
    INCARNATION
        .get_or_init(|| Uuid::new_v4().to_string())
        .as_str()
}

/// On-disk bootstrap identity. `cluster_id` is filled after enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IdentityFile {
    /// File schema version.
    v: u32,
    /// Stable node id (UUID).
    node_id: String,
    /// Cluster this file has enrolled into, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cluster_id: Option<String>,
}

/// Bootstrap identity loaded from [`NODE_IDENTITY_FILE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// Stable node id.
    pub node_id: String,
    /// Cluster binding, once enrollment has succeeded.
    pub cluster_id: Option<String>,
}

/// Durable node row plus the latest observation columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// Stable node id.
    pub node_id: String,
    /// Cluster id stored with the node.
    pub cluster_id: String,
    /// RFC 3339 creation time. Heartbeats do not change it.
    pub created_at: String,
    /// Process incarnation from the latest heartbeat.
    pub incarnation: String,
    /// RFC 3339 heartbeat time.
    pub heartbeat_at: String,
    /// Software version from the latest heartbeat.
    pub software_version: String,
    /// Schema state display from the latest heartbeat.
    pub schema_state: String,
    /// True when the reporting binary accepted the library schema.
    pub compatible: bool,
    /// `available_parallelism`, empty when the probe failed.
    pub logical_cpus: Option<i64>,
    /// Process cgroup `cpu.max` quota in microseconds. Empty when unlimited or missing.
    pub cpu_max_quota_us: Option<i64>,
    /// Process cgroup `cpu.max` period in microseconds. Empty when unlimited or missing.
    pub cpu_max_period_us: Option<i64>,
    /// Process cgroup `memory.max` in bytes. Empty when unlimited or missing.
    pub memory_max_bytes: Option<i64>,
    /// Process cgroup `memory.current` in bytes.
    pub memory_current_bytes: Option<i64>,
    /// Process cgroup `memory.stat` field `anon`.
    pub memory_anon_bytes: Option<i64>,
    /// Free bytes on the filesystem that holds the files directory.
    pub files_dir_free_bytes: Option<i64>,
    /// Byte size of the acquire scratch tree under the download cache.
    pub scratch_bytes: Option<i64>,
    /// Bookclerk artifact target of the reporting binary; empty on a row
    /// written before targets were recorded.
    pub target: String,
    /// Operator-assigned placement labels, sorted.
    pub labels: Vec<String>,
    /// How the node joined the cluster.
    pub kind: NodeKind,
    /// User who owns this node and may administer it, when one is assigned.
    pub owner_user_id: Option<i64>,
}

/// How a node joined the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// Shares the library database and `master.key`. Every node is one today.
    Owner,
    /// Keeps its own database and operator, and enrolled to contribute work.
    Host,
}

impl NodeKind {
    /// Stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Host => "host",
        }
    }

    /// Parses stored text; anything unknown reads as [`Self::Owner`].
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "host" => Self::Host,
            _ => Self::Owner,
        }
    }
}

/// Bookclerk artifact target this binary runs as (`linux-x64-gnu`,
/// `macos-arm64`, …). Matches `bookclerk_plugin_catalog::host_bookclerk_target`
/// (a plugin-host test pins the two together).
#[must_use]
pub fn node_target() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macos-x64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-arm64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x64"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-arm64"
    } else {
        "unknown"
    }
}

/// Capacity sample written on heartbeat. Not desired state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeObservation {
    /// `available_parallelism`, empty when the probe failed.
    pub logical_cpus: Option<i64>,
    /// `cpu.max` quota. Empty when the value is `max` or the file is missing.
    pub cpu_max_quota_us: Option<i64>,
    /// `cpu.max` period. Empty when the value is `max` or the file is missing.
    pub cpu_max_period_us: Option<i64>,
    /// `memory.max`. Empty when the value is `max` or the file is missing.
    pub memory_max_bytes: Option<i64>,
    /// `memory.current`.
    pub memory_current_bytes: Option<i64>,
    /// `memory.stat` `anon`.
    pub memory_anon_bytes: Option<i64>,
    /// `statvfs` free bytes on `files_dir`.
    pub files_dir_free_bytes: Option<i64>,
    /// Bytes under `{scratch}/acquire` and `{scratch}/acquire-pdf`.
    pub scratch_bytes: Option<i64>,
}

/// Path of the node identity file.
#[must_use]
pub fn node_identity_path(files_dir: &Path) -> PathBuf {
    files_dir.join(NODE_IDENTITY_FILE)
}

/// Loads the identity file or creates a new node id.
///
/// Concurrent creators of the same path share the winner's node id.
/// Copying the file copies the node id. A new file is a distinct node.
///
/// # Errors
///
/// Returns an error when the file is unreadable, has an unknown version, or
/// cannot be created.
pub fn load_or_create_node_identity(files_dir: &Path) -> Result<NodeIdentity> {
    let path = node_identity_path(files_dir);
    if path.is_file() {
        return read_identity(&path).map(public_identity);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fresh = IdentityFile {
        v: IDENTITY_FILE_VERSION,
        node_id: Uuid::new_v4().to_string(),
        cluster_id: None,
    };
    match write_identity_create_new(&path, &fresh) {
        Ok(()) => Ok(public_identity(fresh)),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read_identity(&path).map(public_identity)
        }
        Err(err) => Err(err.into()),
    }
}

/// Records `cluster_id` on the identity file.
///
/// A file that already names a different cluster is left unchanged and the
/// call fails. That is the copied-runtime / wrong-database case.
///
/// # Errors
///
/// Returns an error on a cluster mismatch or when the file cannot be replaced.
pub fn bind_cluster_id(files_dir: &Path, cluster_id: &str) -> Result<NodeIdentity> {
    let path = node_identity_path(files_dir);
    let mut identity = read_identity(&path)?;
    match identity.cluster_id.as_deref() {
        Some(existing) if existing == cluster_id => Ok(public_identity(identity)),
        Some(existing) => Err(LibraryError::Conflict(format!(
            "cluster mismatch: node identity is bound to {existing}, database cluster is {cluster_id}"
        ))),
        None => {
            identity.cluster_id = Some(cluster_id.to_string());
            write_identity_atomic(&path, &identity)?;
            Ok(public_identity(identity))
        }
    }
}

/// Preferred cluster id from the identity file, when the file already has one.
///
/// # Errors
///
/// Returns an error when the identity file cannot be read.
pub fn preferred_cluster_id(files_dir: &Path) -> Result<Option<String>> {
    let path = node_identity_path(files_dir);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(read_identity(&path)?.cluster_id)
}

/// Inserts the node when absent and refreshes observation columns.
///
/// `created_at` and `cluster_id` stay at their first values. A heartbeat for a
/// node id already enrolled in another cluster fails and does not rewrite that
/// row.
///
/// # Errors
///
/// Returns an error when the schema is incompatible or the write fails.
pub async fn register_and_heartbeat(
    store: &LibraryStore,
    node_id: &str,
    cluster_id: &str,
    incarnation: &str,
    observation: &NodeObservation,
) -> Result<NodeRecord> {
    if !super::node_queue::is_valid_node_id(node_id) {
        return Err(LibraryError::Invalid(format!(
            "invalid node id `{node_id}`: it must be a lowercase queue-name suffix \
             (a UUID, or `[a-z0-9][a-z0-9._-]*`) short enough for `node.<id>`"
        )));
    }
    let now = Utc::now().to_rfc3339();
    let state = current_schema_state(store.db()).await?;
    let compatible = schema_compatible(&state);
    if !compatible {
        return Err(LibraryError::Schema(format!(
            "host schema {} is not compatible with this binary ({})",
            state.display(),
            expected_schema_label()
        )));
    }
    let schema_state = state.display();
    let version = env!("CARGO_PKG_VERSION");
    let operation_id = format!("node-heartbeat-{node_id}-{incarnation}");
    let reply = batch::execute_host_batch(
        store,
        request(
            &operation_id,
            vec![
                exec(
                    "INSERT OR IGNORE INTO nodes (
                        node_id, cluster_id, created_at, incarnation, heartbeat_at,
                        software_version, schema_state, compatible,
                        logical_cpus, cpu_max_quota_us, cpu_max_period_us,
                        memory_max_bytes, memory_current_bytes, memory_anon_bytes,
                        files_dir_free_bytes, scratch_bytes, target
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    vec![
                        text(node_id),
                        text(cluster_id),
                        text(&now),
                        text(incarnation),
                        text(&now),
                        text(version),
                        text(&schema_state),
                        int(1),
                        int_opt(observation.logical_cpus),
                        int_opt(observation.cpu_max_quota_us),
                        int_opt(observation.cpu_max_period_us),
                        int_opt(observation.memory_max_bytes),
                        int_opt(observation.memory_current_bytes),
                        int_opt(observation.memory_anon_bytes),
                        int_opt(observation.files_dir_free_bytes),
                        int_opt(observation.scratch_bytes),
                        text(node_target()),
                    ],
                ),
                exec(
                    "UPDATE nodes SET
                        incarnation = ?, heartbeat_at = ?, software_version = ?,
                        schema_state = ?, compatible = ?,
                        logical_cpus = ?, cpu_max_quota_us = ?, cpu_max_period_us = ?,
                        memory_max_bytes = ?, memory_current_bytes = ?, memory_anon_bytes = ?,
                        files_dir_free_bytes = ?, scratch_bytes = ?, target = ?
                     WHERE node_id = ? AND cluster_id = ?",
                    vec![
                        text(incarnation),
                        text(&now),
                        text(version),
                        text(&schema_state),
                        int(1),
                        int_opt(observation.logical_cpus),
                        int_opt(observation.cpu_max_quota_us),
                        int_opt(observation.cpu_max_period_us),
                        int_opt(observation.memory_max_bytes),
                        int_opt(observation.memory_current_bytes),
                        int_opt(observation.memory_anon_bytes),
                        int_opt(observation.files_dir_free_bytes),
                        int_opt(observation.scratch_bytes),
                        text(node_target()),
                        text(node_id),
                        text(cluster_id),
                    ],
                ),
            ],
        ),
    )
    .await?;
    if batch::rows_affected(&reply, 1) == 0 {
        let existing = nodes::Entity::find_by_id(node_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let bound = existing
            .as_ref()
            .map(|row| row.cluster_id.as_str())
            .unwrap_or("missing");
        return Err(LibraryError::Conflict(format!(
            "cluster mismatch: node {node_id} is registered in cluster {bound}, not {cluster_id}"
        )));
    }
    load_node(store, node_id).await
}

/// Heartbeats using this process's incarnation.
///
/// # Errors
///
/// Returns an error when registration fails.
pub async fn heartbeat_process(
    store: &LibraryStore,
    node_id: &str,
    cluster_id: &str,
    files_dir: &Path,
    scratch_dir: &Path,
) -> Result<NodeRecord> {
    let cgroup = process_cgroup_dir();
    let files = files_dir.to_path_buf();
    let scratch = scratch_dir.to_path_buf();
    let observation = tokio::task::spawn_blocking(move || {
        sample_node_observation(cgroup.as_deref(), &files, &scratch)
    })
    .await
    .map_err(|err| LibraryError::Other(anyhow::anyhow!("heartbeat sample panicked: {err}")))?;
    register_and_heartbeat(
        store,
        node_id,
        cluster_id,
        process_incarnation(),
        &observation,
    )
    .await
}

/// Reads capacity for a heartbeat.
///
/// `cgroup_dir` overrides the process cgroup so tests can point at fake
/// controller files. `None` records empty cgroup columns.
pub(crate) fn sample_node_observation(
    cgroup_dir: Option<&Path>,
    files_dir: &Path,
    scratch_dir: &Path,
) -> NodeObservation {
    let cgroup = cgroup_dir.map(read_cgroup_sample).unwrap_or_default();
    NodeObservation {
        logical_cpus: logical_cpu_count(),
        cpu_max_quota_us: fit_i64(cgroup.cpu_max_quota_us),
        cpu_max_period_us: fit_i64(cgroup.cpu_max_period_us),
        memory_max_bytes: fit_i64(cgroup.memory_max_bytes),
        memory_current_bytes: fit_i64(cgroup.memory_current_bytes),
        memory_anon_bytes: fit_i64(cgroup.memory_anon_bytes),
        files_dir_free_bytes: filesystem_free_bytes(files_dir)
            .and_then(|bytes| fit_i64(Some(bytes))),
        scratch_bytes: fit_i64(Some(scratch_tree_bytes(scratch_dir))),
    }
}

/// Saturates a byte count into the signed column, or empty when absent.
fn fit_i64(value: Option<u64>) -> Option<i64> {
    value.and_then(|value| i64::try_from(value).ok())
}

/// Bytes in `{scratch}/acquire` and `{scratch}/acquire-pdf`.
///
/// Same trees `scratch_usage` walks. A missing directory counts as zero.
fn scratch_tree_bytes(scratch_dir: &Path) -> u64 {
    dir_bytes(&scratch_dir.join("acquire"))
        .saturating_add(dir_bytes(&scratch_dir.join("acquire-pdf")))
}

/// Directory entries visited by one scratch walk before it returns what it has.
const SCRATCH_WALK_ENTRY_CAP: u64 = 100_000;

/// Recursive file size. Missing paths and unreadable entries count as zero.
///
/// The walk stops after 100_000 entries and returns the bytes counted so far.
fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut seen = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if seen >= SCRATCH_WALK_ENTRY_CAP {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            seen += 1;
            if seen > SCRATCH_WALK_ENTRY_CAP {
                break;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Loads one node row.
///
/// # Errors
///
/// Returns an error when the row is missing or the read fails.
pub async fn load_node(store: &LibraryStore, node_id: &str) -> Result<NodeRecord> {
    let row = nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .ok_or_else(|| LibraryError::NotFound(format!("node {node_id}")))?;
    Ok(record_from_model(row))
}

/// Lists node ids, sorted.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_node_ids(store: &LibraryStore) -> Result<Vec<String>> {
    let rows = nodes::Entity::find()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut ids: Vec<String> = rows.into_iter().map(|row| row.node_id).collect();
    ids.sort();
    Ok(ids)
}

/// Every node row, sorted by id.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_nodes(store: &LibraryStore) -> Result<Vec<NodeRecord>> {
    let mut rows: Vec<NodeRecord> = nodes::Entity::find()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(record_from_model)
        .collect();
    rows.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    Ok(rows)
}

/// This process's incarnation. Stable until the process exits.
#[must_use]
pub fn current_process_incarnation() -> &'static str {
    process_incarnation()
}

/// Projects the on-disk file onto the public identity.
fn public_identity(identity: IdentityFile) -> NodeIdentity {
    NodeIdentity {
        node_id: identity.node_id,
        cluster_id: identity.cluster_id,
    }
}

/// Maps a node row onto the public record.
fn record_from_model(row: nodes::Model) -> NodeRecord {
    NodeRecord {
        node_id: row.node_id,
        cluster_id: row.cluster_id,
        created_at: row.created_at,
        incarnation: row.incarnation,
        heartbeat_at: row.heartbeat_at,
        software_version: row.software_version,
        schema_state: row.schema_state,
        compatible: row.compatible != 0,
        logical_cpus: row.logical_cpus,
        cpu_max_quota_us: row.cpu_max_quota_us,
        cpu_max_period_us: row.cpu_max_period_us,
        memory_max_bytes: row.memory_max_bytes,
        memory_current_bytes: row.memory_current_bytes,
        memory_anon_bytes: row.memory_anon_bytes,
        files_dir_free_bytes: row.files_dir_free_bytes,
        scratch_bytes: row.scratch_bytes,
        target: row.target,
        labels: parse_labels(&row.labels_json),
        kind: NodeKind::parse(&row.kind),
        owner_user_id: row.owner_user_id,
    }
}

/// Stored label JSON as a sorted list; malformed JSON reads as no labels.
pub(super) fn parse_labels(raw: &str) -> Vec<String> {
    let mut labels: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    labels.sort();
    labels.dedup();
    labels
}

/// True when `state` is the unreleased pack this binary applies.
fn schema_compatible(state: &SchemaState) -> bool {
    match state {
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => *base_version == SCHEMA_VERSION && checksum == &unreleased_checksum(),
        SchemaState::Frozen { .. } | SchemaState::Uninitialized => false,
    }
}

/// Schema display this binary accepts on heartbeat.
fn expected_schema_label() -> String {
    format!("unreleased@base{SCHEMA_VERSION}+{}", unreleased_checksum())
}

/// Reads and validates an identity file.
fn read_identity(path: &Path) -> Result<IdentityFile> {
    let raw = std::fs::read_to_string(path)?;
    let parsed: IdentityFile = serde_json::from_str(&raw).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!(
            "node identity file {} is not valid JSON: {err}",
            path.display()
        ))
    })?;
    if parsed.v != IDENTITY_FILE_VERSION {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "unsupported node identity file version {} in {}",
            parsed.v,
            path.display()
        )));
    }
    if Uuid::parse_str(&parsed.node_id).is_err() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "node identity file {} has a non-UUID node_id",
            path.display()
        )));
    }
    Ok(parsed)
}

/// Creates the identity file, failing if it already exists.
fn write_identity_create_new(path: &Path, identity: &IdentityFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(identity)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    write_create_new(path, &bytes)
}

/// Replaces the identity file via a same-directory rename.
fn write_identity_atomic(path: &Path, identity: &IdentityFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(identity)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("node identity encode: {err}")))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}",
        NODE_IDENTITY_FILE,
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    write_create_new(&tmp, &bytes)?;
    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        LibraryError::Io(err)
    })?;
    Ok(())
}

/// Exclusive create with mode `0600` on Unix.
fn write_create_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}
