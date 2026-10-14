//! Plugin releases: what is "installed" is a cluster record, not a node
//! directory.
//!
//! A [`PluginRelease`] row per `(plugin_key, version)` carries the manifest,
//! the settings schema, one artifact per target with its digests, the
//! publisher verification result, and the operator's consent grant. A
//! pointer row names the active release per plugin key. Nodes materialize
//! the active release's artifact into a content-addressed cache and spawn
//! from there; moving the pointer (update, rollback) wakes every node.
//!
//! Small archives may be stored in `plugin_release_blobs` when no shared
//! location exists (the `blob:sha256:<hex>` artifact URL); larger ones stay
//! where the artifact URL points (a registry, an operator bucket, or a
//! shared `file:` path).

use base64::Engine as _;
use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};

use super::documents::ConfigActor;
use super::node_queue::{notify_all_nodes, NodeMessage};
use super::schemas::{pin_release_schema, SchemaPin, SchemaPinOutcome};
use crate::entities::{plugin_release_blobs, plugin_release_pointers, plugin_releases};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Largest archive stored in `plugin_release_blobs` (base64 stays under
/// D1's row cap).
pub const MAX_RELEASE_BLOB_BYTES: usize = 1_048_576;

/// Artifact URL scheme of an archive stored in `plugin_release_blobs`.
pub const RELEASE_BLOB_SCHEME: &str = "blob:sha256:";

/// One downloadable artifact of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseArtifact {
    /// Bookclerk target (`linux-x64-gnu`, …; `any` for workerd packages).
    pub target: String,
    /// Where nodes fetch the archive: `https:`, `file:`, or
    /// `blob:sha256:<hex>`.
    pub url: String,
    /// Archive format (`tar.gz` or `zip`).
    pub format: String,
    /// Path inside the archive to the plugin root.
    pub archive_root: String,
    /// Lowercase hex SHA-256 of the archive bytes.
    pub archive_sha256: String,
    /// Payload-root digest of the extracted plugin tree; the cache directory
    /// is named after it.
    pub payload_root_sha256: String,
    /// Relative path of the executable inside the plugin root.
    pub executable: String,
}

/// One published version of a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRelease {
    /// Canonical plugin key.
    pub plugin_key: String,
    /// Semver package version.
    pub version: String,
    /// Manifest id (display alias).
    pub manifest_id: String,
    /// `plugin.toml` text.
    pub manifest_toml: String,
    /// Settings schema the release declares.
    pub settings: bookclerk_plugin_abi::SettingsSchema,
    /// Per-target artifacts.
    pub artifacts: Vec<ReleaseArtifact>,
    /// True when the publisher was verified at publish time.
    pub publisher_verified: bool,
    /// Approved consent grant (JSON), empty until an operator approves it.
    pub grant_json: String,
    /// Package coordinate the release was resolved from; `update` resolves
    /// newer versions from it. Empty for a local archive.
    pub coordinate: String,
    /// RFC 3339 publish time.
    pub published_at: String,
    /// Audit identity of the publisher.
    pub published_by: String,
}

impl PluginRelease {
    /// The artifact for `target`, falling back to a target-independent
    /// (`any`) artifact.
    #[must_use]
    pub fn artifact_for(&self, target: &str) -> Option<&ReleaseArtifact> {
        self.artifacts
            .iter()
            .find(|a| a.target == target)
            .or_else(|| self.artifacts.iter().find(|a| a.target == "any"))
    }
}

/// The active release pointer of one plugin key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePointer {
    /// Canonical plugin key.
    pub plugin_key: String,
    /// Active version.
    pub version: String,
    /// Compare-and-swap revision.
    pub revision: i64,
    /// RFC 3339 time of the last move.
    pub updated_at: String,
    /// Audit identity of the last writer.
    pub updated_by: String,
}

/// What [`publish_release`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishRelease {
    /// A new release row was written.
    Created(PluginRelease),
    /// The same `(plugin_key, version)` with the same artifacts already
    /// existed; nothing was written.
    Existing(PluginRelease),
}

impl PublishRelease {
    /// The release row, created or existing.
    #[must_use]
    pub fn release(&self) -> &PluginRelease {
        match self {
            Self::Created(r) | Self::Existing(r) => r,
        }
    }
}

/// Only an operator (or process bootstrap, for platform packages) publishes.
fn authorize(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } | ConfigActor::Bootstrap => Ok(()),
        ConfigActor::Scheduler | ConfigActor::Administrator { .. } | ConfigActor::Member { .. } => {
            Err(LibraryError::Other(anyhow::anyhow!(
                "unauthorized configuration write: operator authority is required"
            )))
        }
    }
}

/// Rejects a malformed release before it reaches the table.
fn validate(release: &PluginRelease) -> Result<()> {
    let invalid =
        |detail: String| LibraryError::Other(anyhow::anyhow!("invalid release: {detail}"));
    if release.plugin_key.trim().is_empty() || release.version.trim().is_empty() {
        return Err(invalid("plugin key and version are required".into()));
    }
    if release.artifacts.is_empty() {
        return Err(invalid("at least one artifact is required".into()));
    }
    for artifact in &release.artifacts {
        for (field, value) in [
            ("archive_sha256", &artifact.archive_sha256),
            ("payload_root_sha256", &artifact.payload_root_sha256),
        ] {
            if value.len() != 64
                || !value
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            {
                return Err(invalid(format!("{field} must be lowercase hex sha256")));
            }
        }
        if artifact.url.trim().is_empty() || artifact.executable.trim().is_empty() {
            return Err(invalid("artifact url and executable are required".into()));
        }
    }
    Ok(())
}

/// Encodes a row for the table.
fn to_model(release: &PluginRelease) -> Result<plugin_releases::ActiveModel> {
    let encode =
        |err: serde_json::Error| LibraryError::Other(anyhow::anyhow!("encode release: {err}"));
    Ok(plugin_releases::ActiveModel {
        plugin_key: Set(release.plugin_key.clone()),
        version: Set(release.version.clone()),
        manifest_id: Set(release.manifest_id.clone()),
        manifest_toml: Set(release.manifest_toml.clone()),
        settings_json: Set(serde_json::to_string(&release.settings).map_err(encode)?),
        artifacts_json: Set(serde_json::to_string(&release.artifacts).map_err(encode)?),
        publisher_verified: Set(i64::from(release.publisher_verified)),
        grant_json: Set(release.grant_json.clone()),
        coordinate: Set(release.coordinate.clone()),
        published_at: Set(release.published_at.clone()),
        published_by: Set(release.published_by.clone()),
    })
}

/// Decodes a table row.
fn from_model(row: plugin_releases::Model) -> Result<PluginRelease> {
    let decode =
        |err: serde_json::Error| LibraryError::Other(anyhow::anyhow!("decode release: {err}"));
    Ok(PluginRelease {
        settings: serde_json::from_str(&row.settings_json).map_err(decode)?,
        artifacts: serde_json::from_str(&row.artifacts_json).map_err(decode)?,
        plugin_key: row.plugin_key,
        version: row.version,
        manifest_id: row.manifest_id,
        manifest_toml: row.manifest_toml,
        publisher_verified: row.publisher_verified != 0,
        grant_json: row.grant_json,
        coordinate: row.coordinate,
        published_at: row.published_at,
        published_by: row.published_by,
    })
}

/// Records a release. Publishing the same `(plugin_key, version)` again
/// with the same artifact digests is a no-op ([`PublishRelease::Existing`]);
/// different digests under a published version are refused, so a version
/// never changes content behind a node's cache.
///
/// `published_at` / `published_by` are stamped here.
///
/// # Errors
///
/// Returns an error when the actor may not publish, the release is
/// malformed, the version exists with other content, or the write fails.
pub async fn publish_release(
    store: &LibraryStore,
    actor: &ConfigActor,
    mut release: PluginRelease,
) -> Result<PublishRelease> {
    authorize(actor)?;
    validate(&release)?;
    if let Some(existing) = load_release(store, &release.plugin_key, &release.version).await? {
        return same_content(existing, &release);
    }
    release.published_at = Utc::now().to_rfc3339();
    release.published_by = actor.audit_id().to_string();
    let inserted = plugin_releases::Entity::insert(to_model(&release)?)
        .on_conflict(
            OnConflict::columns([
                plugin_releases::Column::PluginKey,
                plugin_releases::Column::Version,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let stored = load_release(store, &release.plugin_key, &release.version)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("release {}", release.plugin_key)))?;
    if inserted == 0 {
        // Another publisher won the race; decide against its row.
        return same_content(stored, &release);
    }
    Ok(PublishRelease::Created(stored))
}

/// Existing row for a re-publish: identical artifact digests or a conflict.
fn same_content(existing: PluginRelease, offered: &PluginRelease) -> Result<PublishRelease> {
    let digests = |r: &PluginRelease| {
        let mut d: Vec<(String, String, String)> = r
            .artifacts
            .iter()
            .map(|a| {
                (
                    a.target.clone(),
                    a.archive_sha256.clone(),
                    a.payload_root_sha256.clone(),
                )
            })
            .collect();
        d.sort();
        d
    };
    if digests(&existing) == digests(offered) && existing.manifest_toml == offered.manifest_toml {
        Ok(PublishRelease::Existing(existing))
    } else {
        Err(LibraryError::Conflict(format!(
            "release {} {} is already published with different content; publish a new version",
            offered.plugin_key, offered.version
        )))
    }
}

/// One release row.
///
/// # Errors
///
/// Returns an error when the read or decode fails.
pub async fn load_release(
    store: &LibraryStore,
    plugin_key: &str,
    version: &str,
) -> Result<Option<PluginRelease>> {
    plugin_releases::Entity::find_by_id((plugin_key.to_string(), version.to_string()))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(from_model)
        .transpose()
}

/// Every release of `plugin_key` (or of every plugin), newest publish first.
///
/// # Errors
///
/// Returns an error when the read or decode fails.
pub async fn list_releases(
    store: &LibraryStore,
    plugin_key: Option<&str>,
) -> Result<Vec<PluginRelease>> {
    let mut query = plugin_releases::Entity::find();
    if let Some(key) = plugin_key {
        query = query.filter(plugin_releases::Column::PluginKey.eq(key));
    }
    query
        .order_by_desc(plugin_releases::Column::PublishedAt)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(from_model)
        .collect()
}

/// The active pointer of `plugin_key`.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_release_pointer(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<ReleasePointer>> {
    Ok(
        plugin_release_pointers::Entity::find_by_id(plugin_key.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|row| ReleasePointer {
                plugin_key: row.plugin_key,
                version: row.version,
                revision: row.revision,
                updated_at: row.updated_at,
                updated_by: row.updated_by,
            }),
    )
}

/// Every active pointer.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_release_pointers(store: &LibraryStore) -> Result<Vec<ReleasePointer>> {
    Ok(plugin_release_pointers::Entity::find()
        .order_by_asc(plugin_release_pointers::Column::PluginKey)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|row| ReleasePointer {
            plugin_key: row.plugin_key,
            version: row.version,
            revision: row.revision,
            updated_at: row.updated_at,
            updated_by: row.updated_by,
        })
        .collect())
}

/// The active release of `plugin_key`, when a pointer names one.
///
/// # Errors
///
/// Returns an error when a read fails or the pointer names a missing row.
pub async fn load_active_release(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<PluginRelease>> {
    let Some(pointer) = load_release_pointer(store, plugin_key).await? else {
        return Ok(None);
    };
    load_release(store, plugin_key, &pointer.version)
        .await?
        .map(Some)
        .ok_or_else(|| {
            LibraryError::NotFound(format!(
                "active release {plugin_key} {} is missing",
                pointer.version
            ))
        })
}

/// Moves the active pointer of `plugin_key` to `version` (first install,
/// update, or rollback), pins the release's settings schema, and wakes every
/// node. Pointing at the version that is already active is a no-op.
///
/// # Errors
///
/// Returns an error when the actor may not write, the release does not
/// exist, or a write fails.
pub async fn activate_release(
    store: &LibraryStore,
    actor: &ConfigActor,
    plugin_key: &str,
    version: &str,
    node_id: &str,
) -> Result<(ReleasePointer, SchemaPinOutcome)> {
    authorize(actor)?;
    let release = load_release(store, plugin_key, version)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("release {plugin_key} {version}")))?;
    let pinned = pin_release_schema(
        store,
        &SchemaPin {
            plugin_key,
            manifest_id: &release.manifest_id,
            package_version: &release.version,
            schema: &release.settings,
            node_id,
        },
    )
    .await?;
    let current = load_release_pointer(store, plugin_key).await?;
    if current.as_ref().is_some_and(|p| p.version == version) {
        return Ok((current.expect("checked above"), pinned));
    }
    let revision = current.as_ref().map_or(1, |p| p.revision + 1);
    let model = plugin_release_pointers::ActiveModel {
        plugin_key: Set(plugin_key.to_string()),
        version: Set(version.to_string()),
        revision: Set(revision),
        updated_at: Set(Utc::now().to_rfc3339()),
        updated_by: Set(actor.audit_id().to_string()),
    };
    plugin_release_pointers::Entity::insert(model)
        .on_conflict(
            OnConflict::column(plugin_release_pointers::Column::PluginKey)
                .update_columns([
                    plugin_release_pointers::Column::Version,
                    plugin_release_pointers::Column::Revision,
                    plugin_release_pointers::Column::UpdatedAt,
                    plugin_release_pointers::Column::UpdatedBy,
                ])
                .to_owned(),
        )
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let pointer = load_release_pointer(store, plugin_key)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("release pointer {plugin_key}")))?;
    notify_all_nodes(
        store,
        NodeMessage::ReleaseChanged {
            plugin_key: plugin_key.to_string(),
            version: pointer.version.clone(),
            revision: pointer.revision,
        },
    )
    .await;
    Ok((pointer, pinned))
}

/// Clears the active pointer (`plugins remove`). Release rows stay so the
/// plugin can be re-activated; nodes retire guests whose plugin has no
/// active release and sweep the cache.
///
/// # Errors
///
/// Returns an error when the actor may not write or the delete fails.
pub async fn clear_active_release(
    store: &LibraryStore,
    actor: &ConfigActor,
    plugin_key: &str,
) -> Result<bool> {
    authorize(actor)?;
    let result = plugin_release_pointers::Entity::delete_by_id(plugin_key.to_string())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if result.rows_affected > 0 {
        notify_all_nodes(
            store,
            NodeMessage::ReleaseChanged {
                plugin_key: plugin_key.to_string(),
                version: String::new(),
                revision: 0,
            },
        )
        .await;
    }
    Ok(result.rows_affected > 0)
}

/// Records the operator-approved consent grant on a release.
///
/// # Errors
///
/// Returns an error when the actor may not write, the release is missing,
/// or the write fails.
pub async fn approve_release_grant(
    store: &LibraryStore,
    actor: &ConfigActor,
    plugin_key: &str,
    version: &str,
    grant_json: &str,
) -> Result<PluginRelease> {
    authorize(actor)?;
    let updated = plugin_releases::Entity::update_many()
        .col_expr(
            plugin_releases::Column::GrantJson,
            sea_orm::sea_query::Expr::value(grant_json.to_string()),
        )
        .filter(plugin_releases::Column::PluginKey.eq(plugin_key))
        .filter(plugin_releases::Column::Version.eq(version))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if updated.rows_affected == 0 {
        return Err(LibraryError::NotFound(format!(
            "release {plugin_key} {version}"
        )));
    }
    let release = load_release(store, plugin_key, version)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("release {plugin_key} {version}")))?;
    notify_all_nodes(
        store,
        NodeMessage::ReleaseChanged {
            plugin_key: plugin_key.to_string(),
            version: version.to_string(),
            revision: 0,
        },
    )
    .await;
    Ok(release)
}

/// Stores a small archive and returns its `blob:sha256:<hex>` URL.
///
/// # Errors
///
/// Returns an error when the archive exceeds [`MAX_RELEASE_BLOB_BYTES`] or
/// the write fails.
pub async fn put_release_blob(store: &LibraryStore, bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_RELEASE_BLOB_BYTES {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "archive of {} bytes exceeds the {MAX_RELEASE_BLOB_BYTES}-byte library blob cap; \
             publish it from a shared location instead",
            bytes.len()
        )));
    }
    let sha = {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let model = plugin_release_blobs::ActiveModel {
        sha256: Set(sha.clone()),
        data: Set(base64::engine::general_purpose::STANDARD.encode(bytes)),
        size: Set(i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
        created_at: Set(Utc::now().to_rfc3339()),
    };
    plugin_release_blobs::Entity::insert(model)
        .on_conflict(
            OnConflict::column(plugin_release_blobs::Column::Sha256)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(format!("{RELEASE_BLOB_SCHEME}{sha}"))
}

/// Reads an archive stored by [`put_release_blob`].
///
/// # Errors
///
/// Returns an error when the read fails or the stored bytes do not match
/// their digest.
pub async fn load_release_blob(store: &LibraryStore, sha256: &str) -> Result<Option<Vec<u8>>> {
    let Some(row) = plugin_release_blobs::Entity::find_by_id(sha256.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(row.data.as_bytes())
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("release blob is not base64: {err}")))?;
    let actual = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    if actual != sha256 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "release blob {sha256} does not match its digest"
        )));
    }
    Ok(Some(bytes))
}
