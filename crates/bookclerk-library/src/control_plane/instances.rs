//! Durable plugin instance identity.
//!
//! [`PluginInstanceId`] is minted once. Restart, alias text, entrypoint list,
//! capability list, and [`HostId`](crate::control_plane::HostIdentity) do not
//! change it. Two instances may name the same canonical plugin key.

use chrono::Utc;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use uuid::Uuid;

use super::documents::ConfigActor;
use crate::entities::plugin_instances;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Stable id of one plugin instance.
///
/// A hyphenated UUID version 4. It does not parse as a plugin key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginInstanceId(String);

impl PluginInstanceId {
    /// Mints a new random id.
    #[must_use]
    pub fn mint() -> Self {
        Self(Uuid::new_v4().hyphenated().to_string())
    }

    /// Parses a hyphenated UUID.
    ///
    /// # Errors
    ///
    /// Returns an error when `raw` is not a UUID.
    pub fn parse(raw: &str) -> Result<Self> {
        let id = Uuid::parse_str(raw.trim()).map_err(|_| {
            LibraryError::Other(anyhow::anyhow!(
                "invalid plugin instance id: expected a UUID"
            ))
        })?;
        Ok(Self(id.hyphenated().to_string()))
    }

    /// Hyphenated UUID text stored in the database.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PluginInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `plugin_instances` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstance {
    /// Stable id.
    pub id: PluginInstanceId,
    /// Canonical plugin key. Immutable after create.
    pub plugin_key: String,
    /// RFC 3339 insert time.
    pub created_at: String,
    /// Audit identity of the creator.
    pub created_by: String,
}

/// Inserts a new instance for `plugin_key`.
///
/// A second call mints a different id even when `plugin_key` matches.
///
/// # Errors
///
/// Returns an error when the actor may not create an instance or the insert fails.
pub async fn create_plugin_instance(
    store: &LibraryStore,
    actor: &ConfigActor,
    plugin_key: &str,
) -> Result<PluginInstance> {
    authorize_create(actor)?;
    validate_plugin_key(plugin_key)?;
    let id = PluginInstanceId::mint();
    insert_instance(store, actor, &id, plugin_key).await
}

/// Returns the existing instance for `plugin_key`, or inserts one.
///
/// GraphicAudio startup uses this so a second process start keeps the original id.
/// Explicit [`create_plugin_instance`] can still mint another row for the same key.
///
/// # Errors
///
/// Returns an error when the actor may not create an instance or the write fails.
pub async fn ensure_plugin_instance(
    store: &LibraryStore,
    actor: &ConfigActor,
    plugin_key: &str,
) -> Result<PluginInstance> {
    authorize_create(actor)?;
    validate_plugin_key(plugin_key)?;
    if let Some(existing) = find_by_plugin_key(store, plugin_key).await? {
        return Ok(existing);
    }
    let id = PluginInstanceId::mint();
    match insert_instance(store, actor, &id, plugin_key).await {
        Ok(row) => Ok(row),
        Err(err) => {
            if let Some(existing) = find_by_plugin_key(store, plugin_key).await? {
                return Ok(existing);
            }
            Err(err)
        }
    }
}

/// Loads one instance by id.
///
/// # Errors
///
/// Returns an error when the read fails or `id` is not a UUID.
pub async fn load_plugin_instance(
    store: &LibraryStore,
    id: &PluginInstanceId,
) -> Result<Option<PluginInstance>> {
    let row = plugin_instances::Entity::find_by_id(id.as_str().to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(row.map(instance_from_model))
}

/// Earliest instance row for `plugin_key`, if any.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn find_plugin_instance_by_key(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<PluginInstance>> {
    find_by_plugin_key(store, plugin_key).await
}

/// Inserts one row. The id is the caller's.
///
/// # Errors
///
/// Returns an error when the insert fails.
async fn insert_instance(
    store: &LibraryStore,
    actor: &ConfigActor,
    id: &PluginInstanceId,
    plugin_key: &str,
) -> Result<PluginInstance> {
    let now = Utc::now().to_rfc3339();
    let created_by = actor.audit_id().to_string();
    let model = plugin_instances::ActiveModel {
        plugin_instance_id: Set(id.as_str().to_string()),
        plugin_key: Set(plugin_key.to_string()),
        created_at: Set(now),
        created_by: Set(created_by),
    };
    plugin_instances::Entity::insert(model)
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    load_plugin_instance(store, id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {id}")))
}

/// Oldest row for a plugin key.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn find_by_plugin_key(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<PluginInstance>> {
    let row = plugin_instances::Entity::find()
        .filter(plugin_instances::Column::PluginKey.eq(plugin_key))
        .order_by_asc(plugin_instances::Column::CreatedAt)
        .order_by_asc(plugin_instances::Column::PluginInstanceId)
        .limit(1)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(row.map(instance_from_model))
}

/// Maps a SeaORM row onto the domain instance.
fn instance_from_model(row: plugin_instances::Model) -> PluginInstance {
    PluginInstance {
        id: PluginInstanceId(row.plugin_instance_id),
        plugin_key: row.plugin_key,
        created_at: row.created_at,
        created_by: row.created_by,
    }
}

/// Operator and bootstrap may mint an instance. Portal roles may not.
fn authorize_create(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } | ConfigActor::Bootstrap => Ok(()),
        ConfigActor::Administrator { .. } | ConfigActor::Member { .. } => Err(LibraryError::Other(
            anyhow::anyhow!("unauthorized configuration write: operator authority is required"),
        )),
    }
}

/// Rejects an empty or oversized plugin key before insert.
fn validate_plugin_key(plugin_key: &str) -> Result<()> {
    if plugin_key.is_empty() || plugin_key.len() > 512 || plugin_key.contains('\u{0000}') {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "invalid plugin instance: plugin_key must be 1..=512 characters without NUL"
        )));
    }
    Ok(())
}
