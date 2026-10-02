//! Durable plugin instance identity.
//!
//! [`PluginInstanceId`] is minted once. Restart, alias text, entrypoint list,
//! capability list, and [`HostId`](crate::control_plane::HostIdentity) do not
//! change it. Two instances may name the same canonical plugin key.

use chrono::Utc;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Statement, TransactionTrait, Value,
};
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
    insert_instance(store.db(), actor, &id, plugin_key).await?;
    load_plugin_instance(store, &id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {id}")))
}

/// Returns the existing instance for `plugin_key`, or inserts one.
///
/// GraphicAudio startup uses this so a second process start keeps the original id.
/// Concurrent calls share one inserted row. Explicit [`create_plugin_instance`]
/// can still mint another row for the same key; that path does not claim the
/// default enrollment.
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
    for _ in 0..4 {
        if let Some(existing) = enrolled_or_oldest(store.db(), plugin_key).await? {
            return Ok(existing);
        }
        let id = PluginInstanceId::mint();
        let txn = store.db().begin().await.map_err(LibraryError::Orm)?;
        if let Some(existing) = enrolled_or_oldest(&txn, plugin_key).await? {
            txn.rollback().await.map_err(LibraryError::Orm)?;
            return Ok(existing);
        }
        insert_instance(&txn, actor, &id, plugin_key).await?;
        match insert_default_enrollment(&txn, plugin_key, id.as_str()).await {
            Ok(()) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                return load_plugin_instance(store, &id)
                    .await?
                    .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {id}")));
            }
            Err(err) if enrollment_contention(&err) => {
                let _ = txn.rollback().await;
            }
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    }
    enrolled_or_oldest(store.db(), plugin_key)
        .await?
        .ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!(
                "plugin instance enrollment for `{plugin_key}` lost the insert race"
            ))
        })
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

/// Every instance row for `plugin_key`, oldest first.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_plugin_instances_for_key(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Vec<PluginInstance>> {
    let rows = plugin_instances::Entity::find()
        .filter(plugin_instances::Column::PluginKey.eq(plugin_key))
        .order_by_asc(plugin_instances::Column::CreatedAt)
        .order_by_asc(plugin_instances::Column::PluginInstanceId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows.into_iter().map(instance_from_model).collect())
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
    conn: &impl ConnectionTrait,
    actor: &ConfigActor,
    id: &PluginInstanceId,
    plugin_key: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let created_by = actor.audit_id().to_string();
    let model = plugin_instances::ActiveModel {
        plugin_instance_id: Set(id.as_str().to_string()),
        plugin_key: Set(plugin_key.to_string()),
        created_at: Set(now),
        created_by: Set(created_by),
    };
    plugin_instances::Entity::insert(model)
        .exec(conn)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Claims the default enrollment for `plugin_key`.
///
/// The primary key is the plugin key, so a second concurrent bootstrap cannot
/// insert a different default instance. Explicit instance creation does not
/// call this.
async fn insert_default_enrollment(
    conn: &impl ConnectionTrait,
    plugin_key: &str,
    plugin_instance_id: &str,
) -> Result<()> {
    let backend = conn.get_database_backend();
    conn.execute_raw(Statement::from_sql_and_values(
        backend,
        "INSERT INTO plugin_instance_defaults (plugin_key, plugin_instance_id) VALUES ($1, $2)",
        vec![
            Value::from(plugin_key.to_string()),
            Value::from(plugin_instance_id.to_string()),
        ],
    ))
    .await
    .map_err(LibraryError::Orm)?;
    Ok(())
}

/// The enrolled default, or the oldest row when enrollment has not run.
async fn enrolled_or_oldest(
    conn: &impl ConnectionTrait,
    plugin_key: &str,
) -> Result<Option<PluginInstance>> {
    if let Some(id) = default_instance_id(conn, plugin_key).await? {
        let parsed = PluginInstanceId::parse(&id)?;
        if let Some(instance) = load_plugin_instance_conn(conn, &parsed).await? {
            return Ok(Some(instance));
        }
    }
    find_by_plugin_key_conn(conn, plugin_key).await
}

/// Enrolled default instance id for `plugin_key`, when one was claimed.
async fn default_instance_id(
    conn: &impl ConnectionTrait,
    plugin_key: &str,
) -> Result<Option<String>> {
    let backend = conn.get_database_backend();
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            backend,
            "SELECT plugin_instance_id FROM plugin_instance_defaults WHERE plugin_key = $1",
            vec![Value::from(plugin_key.to_string())],
        ))
        .await
        .map_err(LibraryError::Orm)?;
    match row {
        Some(row) => Ok(Some(
            row.try_get("", "plugin_instance_id")
                .map_err(LibraryError::Orm)?,
        )),
        None => Ok(None),
    }
}

/// Loads one instance by id on `conn`, including inside an open transaction.
async fn load_plugin_instance_conn(
    conn: &impl ConnectionTrait,
    id: &PluginInstanceId,
) -> Result<Option<PluginInstance>> {
    let row = plugin_instances::Entity::find_by_id(id.as_str().to_string())
        .one(conn)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(row.map(instance_from_model))
}

/// Oldest instance row for `plugin_key` on `conn`.
async fn find_by_plugin_key_conn(
    conn: &impl ConnectionTrait,
    plugin_key: &str,
) -> Result<Option<PluginInstance>> {
    let row = plugin_instances::Entity::find()
        .filter(plugin_instances::Column::PluginKey.eq(plugin_key))
        .order_by_asc(plugin_instances::Column::CreatedAt)
        .order_by_asc(plugin_instances::Column::PluginInstanceId)
        .limit(1)
        .one(conn)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(row.map(instance_from_model))
}

/// True when another bootstrap claimed the default enrollment or the write lock.
fn enrollment_contention(err: &LibraryError) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("unique")
        || text.contains("duplicate")
        || text.contains("busy")
        || text.contains("locked")
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
