//! Stable host identity file and database registration.
//!
//! The identity file is bootstrap material. Hostname text is never an input.
//! A process incarnation is memory-only and is written as a heartbeat observation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::Utc;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::batch::{self, exec, int, request, text};
use crate::entities::hosts;
use crate::error::{LibraryError, Result};
use crate::host_schema::current_schema_state;
use crate::migrations::{unreleased_checksum, SCHEMA_VERSION};
use crate::schema_state::SchemaState;
use crate::store::LibraryStore;

/// Bootstrap file under `$BOOKCLERK_FILES_DIR` that stores the stable host id.
pub const HOST_IDENTITY_FILE: &str = "host-identity.json";

/// Identity file schema. Unknown versions fail closed.
const IDENTITY_FILE_VERSION: u32 = 1;

/// Process-local incarnation. A restart mints a new value; the host id does not.
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
    /// Stable host id (UUID).
    host_id: String,
    /// Cluster this file has enrolled into, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cluster_id: Option<String>,
}

/// Bootstrap identity loaded from [`HOST_IDENTITY_FILE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    /// Stable host id.
    pub host_id: String,
    /// Cluster binding, once enrollment has succeeded.
    pub cluster_id: Option<String>,
}

/// Durable host row plus the latest observation columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRecord {
    /// Stable host id.
    pub host_id: String,
    /// Cluster id stored with the host.
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
}

/// Path of the host identity file.
#[must_use]
pub fn host_identity_path(files_dir: &Path) -> PathBuf {
    files_dir.join(HOST_IDENTITY_FILE)
}

/// Loads the identity file or creates a new host id.
///
/// Concurrent creators of the same path share the winner's host id.
/// Copying the file copies the host id. A new file is a distinct host.
///
/// # Errors
///
/// Returns an error when the file is unreadable, has an unknown version, or
/// cannot be created.
pub fn load_or_create_host_identity(files_dir: &Path) -> Result<HostIdentity> {
    let path = host_identity_path(files_dir);
    if path.is_file() {
        return read_identity(&path).map(public_identity);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fresh = IdentityFile {
        v: IDENTITY_FILE_VERSION,
        host_id: Uuid::new_v4().to_string(),
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
pub fn bind_cluster_id(files_dir: &Path, cluster_id: &str) -> Result<HostIdentity> {
    let path = host_identity_path(files_dir);
    let mut identity = read_identity(&path)?;
    match identity.cluster_id.as_deref() {
        Some(existing) if existing == cluster_id => Ok(public_identity(identity)),
        Some(existing) => Err(LibraryError::Conflict(format!(
            "cluster mismatch: host identity is bound to {existing}, database cluster is {cluster_id}"
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
    let path = host_identity_path(files_dir);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(read_identity(&path)?.cluster_id)
}

/// Inserts the host when absent and refreshes observation columns.
///
/// `created_at` and `cluster_id` stay at their first values. A heartbeat for a
/// host id already enrolled in another cluster fails and does not rewrite that
/// row.
///
/// # Errors
///
/// Returns an error when the schema is incompatible or the write fails.
pub async fn register_and_heartbeat(
    store: &LibraryStore,
    host_id: &str,
    cluster_id: &str,
    incarnation: &str,
) -> Result<HostRecord> {
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
    let operation_id = format!("host-heartbeat-{host_id}-{incarnation}");
    let reply = batch::execute_host_batch(
        store,
        request(
            &operation_id,
            vec![
                exec(
                    "INSERT OR IGNORE INTO hosts (
                        host_id, cluster_id, created_at, incarnation, heartbeat_at,
                        software_version, schema_state, compatible
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    vec![
                        text(host_id),
                        text(cluster_id),
                        text(&now),
                        text(incarnation),
                        text(&now),
                        text(version),
                        text(&schema_state),
                        int(1),
                    ],
                ),
                exec(
                    "UPDATE hosts SET
                        incarnation = ?, heartbeat_at = ?, software_version = ?,
                        schema_state = ?, compatible = ?
                     WHERE host_id = ? AND cluster_id = ?",
                    vec![
                        text(incarnation),
                        text(&now),
                        text(version),
                        text(&schema_state),
                        int(1),
                        text(host_id),
                        text(cluster_id),
                    ],
                ),
            ],
        ),
    )
    .await?;
    if batch::rows_affected(&reply, 1) == 0 {
        let existing = hosts::Entity::find_by_id(host_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let bound = existing
            .as_ref()
            .map(|row| row.cluster_id.as_str())
            .unwrap_or("missing");
        return Err(LibraryError::Conflict(format!(
            "cluster mismatch: host {host_id} is registered in cluster {bound}, not {cluster_id}"
        )));
    }
    load_host(store, host_id).await
}

/// Heartbeats using this process's incarnation.
///
/// # Errors
///
/// Returns an error when registration fails.
pub async fn heartbeat_process(
    store: &LibraryStore,
    host_id: &str,
    cluster_id: &str,
) -> Result<HostRecord> {
    register_and_heartbeat(store, host_id, cluster_id, process_incarnation()).await
}

/// Loads one host row.
///
/// # Errors
///
/// Returns an error when the row is missing or the read fails.
pub async fn load_host(store: &LibraryStore, host_id: &str) -> Result<HostRecord> {
    let row = hosts::Entity::find_by_id(host_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .ok_or_else(|| LibraryError::NotFound(format!("host {host_id}")))?;
    Ok(record_from_model(row))
}

/// Lists host ids. Used to prove one heartbeat does not delete another host.
///
/// # Errors
///
/// Returns an error when the read fails.
#[cfg(test)]
pub async fn list_host_ids(store: &LibraryStore) -> Result<Vec<String>> {
    let rows = hosts::Entity::find()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut ids: Vec<String> = rows.into_iter().map(|row| row.host_id).collect();
    ids.sort();
    Ok(ids)
}

/// This process's incarnation. Stable until the process exits.
#[must_use]
pub fn current_process_incarnation() -> &'static str {
    process_incarnation()
}

/// Projects the on-disk file onto the public identity.
fn public_identity(identity: IdentityFile) -> HostIdentity {
    HostIdentity {
        host_id: identity.host_id,
        cluster_id: identity.cluster_id,
    }
}

/// Maps a host row onto the public record.
fn record_from_model(row: hosts::Model) -> HostRecord {
    HostRecord {
        host_id: row.host_id,
        cluster_id: row.cluster_id,
        created_at: row.created_at,
        incarnation: row.incarnation,
        heartbeat_at: row.heartbeat_at,
        software_version: row.software_version,
        schema_state: row.schema_state,
        compatible: row.compatible != 0,
    }
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
            "host identity file {} is not valid JSON: {err}",
            path.display()
        ))
    })?;
    if parsed.v != IDENTITY_FILE_VERSION {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "unsupported host identity file version {} in {}",
            parsed.v,
            path.display()
        )));
    }
    if Uuid::parse_str(&parsed.host_id).is_err() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "host identity file {} has a non-UUID host_id",
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
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("host identity encode: {err}")))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}",
        HOST_IDENTITY_FILE,
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
