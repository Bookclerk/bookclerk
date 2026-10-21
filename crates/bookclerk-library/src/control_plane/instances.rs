//! Durable plugin instance identity.
//!
//! [`PluginInstanceId`] is minted once. Restart, alias text, entrypoint list,
//! capability list, and [`NodeId`](crate::control_plane::NodeIdentity) do not
//! change it. Two instances may name the same canonical plugin key.

use chrono::Utc;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use uuid::Uuid;

use super::documents::ConfigActor;
use crate::entities::plugin_instances;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Stable id of one plugin instance.
///
/// A hyphenated UUID version 4. It does not parse as a plugin key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
            LibraryError::Invalid("invalid plugin instance id: expected a UUID".into())
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
    /// Operator-facing name; empty for the default instance.
    pub display_name: String,
    /// RFC 3339 insert time.
    pub created_at: String,
    /// Audit identity of the creator.
    pub created_by: String,
    /// The node that owns this instance (a node-local output, H-2); it may
    /// be deployed only there. `None` for cluster instances.
    pub owner_node_id: Option<String>,
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
    display_name: &str,
) -> Result<PluginInstance> {
    authorize_create(actor)?;
    validate_plugin_key(plugin_key)?;
    validate_display_name(display_name)?;
    let id = PluginInstanceId::mint();
    insert_instance(store.db(), actor, &id, plugin_key, display_name, None).await?;
    load_plugin_instance(store, &id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {id}")))
}

/// Inserts a node-local instance of `plugin_key` owned by `node_id` (H-2):
/// it adds to the cluster's outputs on that node only, and a deployment of
/// it to any other node, or a placement, is refused.
///
/// # Errors
///
/// Returns an error when the actor may not create an instance, the node id
/// is malformed or has no `nodes` row, or the insert fails.
pub async fn create_node_local_instance(
    store: &LibraryStore,
    actor: &ConfigActor,
    plugin_key: &str,
    node_id: &str,
    display_name: &str,
) -> Result<PluginInstance> {
    authorize_create(actor)?;
    validate_plugin_key(plugin_key)?;
    validate_display_name(display_name)?;
    super::deployments::validate_node_id(node_id)?;
    if crate::entities::nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_none()
    {
        return Err(LibraryError::NotFound(format!("node {node_id}")));
    }
    let id = PluginInstanceId::mint();
    insert_instance(
        store.db(),
        actor,
        &id,
        plugin_key,
        display_name,
        Some(node_id),
    )
    .await?;
    load_plugin_instance(store, &id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {id}")))
}

/// Returns the existing instance for `plugin_key`, or inserts one.
///
/// GraphicAudio startup uses this so a second process start keeps the original id.
/// Concurrent calls, including two connections to the same file, share one
/// inserted row. The claim is one canonical host batch (no interactive
/// `BEGIN`), so D1 and other non-interactive adapters can run it. A lock or
/// uniqueness failure retries that batch. Explicit [`create_plugin_instance`]
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
        match enrolled_or_oldest(store, plugin_key).await {
            Ok(Some(existing)) => return Ok(existing),
            Ok(None) => {}
            Err(err) if enrollment_contention(&err) => continue,
            Err(err) => return Err(err),
        }
        let id = PluginInstanceId::mint();
        let now = Utc::now().to_rfc3339();
        let operation_id = format!("enroll-plugin-instance-{}", id.as_str());
        match super::batch::execute_host_batch(
            store,
            super::batch::request(
                &operation_id,
                vec![
                    super::batch::exec(
                        "DELETE FROM plugin_instance_defaults
                         WHERE plugin_key = ?
                           AND NOT EXISTS (
                               SELECT 1 FROM plugin_instances
                                WHERE plugin_instances.plugin_instance_id
                                      = plugin_instance_defaults.plugin_instance_id
                           )",
                        vec![super::batch::text(plugin_key)],
                    ),
                    super::batch::exec(
                        "INSERT INTO plugin_instances (
                            plugin_instance_id, plugin_key, display_name, created_at, created_by
                         )
                         SELECT ?, ?, '', ?, ?
                          WHERE NOT EXISTS (
                              SELECT 1 FROM plugin_instance_defaults WHERE plugin_key = ?
                          )",
                        vec![
                            super::batch::text(id.as_str()),
                            super::batch::text(plugin_key),
                            super::batch::text(&now),
                            super::batch::text(actor.audit_id()),
                            super::batch::text(plugin_key),
                        ],
                    ),
                    super::batch::exec(
                        "INSERT INTO plugin_instance_defaults (plugin_key, plugin_instance_id)
                         SELECT ?, ?
                          WHERE EXISTS (
                              SELECT 1 FROM plugin_instances WHERE plugin_instance_id = ?
                          )
                            AND NOT EXISTS (
                              SELECT 1 FROM plugin_instance_defaults WHERE plugin_key = ?
                          )",
                        vec![
                            super::batch::text(plugin_key),
                            super::batch::text(id.as_str()),
                            super::batch::text(id.as_str()),
                            super::batch::text(plugin_key),
                        ],
                    ),
                ],
            ),
        )
        .await
        {
            Ok(reply) => {
                if super::batch::rows_affected(&reply, 2) > 0 {
                    return load_plugin_instance(store, &id)
                        .await?
                        .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {id}")));
                }
            }
            Err(err) if enrollment_contention(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    enrolled_or_oldest(store, plugin_key).await?.ok_or_else(|| {
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
/// Oldest-wins. Prefer [`default_instance_for_key`], which honours the
/// `plugin_instance_defaults` claim and falls back to this order.
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

/// The instance a host routes to when a job does not pin one: the
/// `plugin_instance_defaults` claim for `plugin_key`, else the oldest row.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn default_instance_for_key(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<PluginInstance>> {
    enrolled_or_oldest(store, plugin_key).await
}

/// Every instance row, ordered by plugin key then creation time.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_all_plugin_instances(store: &LibraryStore) -> Result<Vec<PluginInstance>> {
    let rows = plugin_instances::Entity::find()
        .order_by_asc(plugin_instances::Column::PluginKey)
        .order_by_asc(plugin_instances::Column::CreatedAt)
        .order_by_asc(plugin_instances::Column::PluginInstanceId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows.into_iter().map(instance_from_model).collect())
}

/// Deletes an instance that has no deployments. Its configuration document,
/// audit rows, and sealed secrets are left for retention cleanup.
///
/// # Errors
///
/// Returns an error when the actor may not write, a deployment still
/// references the instance, or the delete fails.
pub async fn delete_plugin_instance(
    store: &LibraryStore,
    actor: &ConfigActor,
    id: &PluginInstanceId,
) -> Result<()> {
    authorize_create(actor)?;
    if super::deployments::instance_has_deployment(store, id).await? {
        return Err(LibraryError::Conflict(format!(
            "plugin instance {id} still has deployments; set them absent and remove them first"
        )));
    }
    bookclerk_db_exec::execute_canonical(
        store.db(),
        "DELETE FROM plugin_instance_defaults WHERE plugin_instance_id = ?",
        [id.as_str().into()],
    )
    .await
    .map_err(LibraryError::Orm)?;
    crate::entities::plugin_placements::Entity::delete_by_id(id.as_str().to_string())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    plugin_instances::Entity::delete_by_id(id.as_str().to_string())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
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
    display_name: &str,
    owner_node_id: Option<&str>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let created_by = actor.audit_id().to_string();
    let model = plugin_instances::ActiveModel {
        plugin_instance_id: Set(id.as_str().to_string()),
        plugin_key: Set(plugin_key.to_string()),
        display_name: Set(display_name.trim().to_string()),
        created_at: Set(now),
        created_by: Set(created_by),
        owner_node_id: Set(owner_node_id.map(str::to_string)),
    };
    plugin_instances::Entity::insert(model)
        .exec(conn)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// The enrolled default, or the oldest row when enrollment has not run.
async fn enrolled_or_oldest(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<PluginInstance>> {
    if let Some(id) = default_instance_id(store, plugin_key).await? {
        let parsed = PluginInstanceId::parse(&id)?;
        if let Some(instance) = load_plugin_instance_conn(store.db(), &parsed).await? {
            return Ok(Some(instance));
        }
    }
    find_by_plugin_key_conn(store.db(), plugin_key).await
}

/// Enrolled default instance id for `plugin_key`, when one was claimed.
///
/// Canonical `?` SQL: lowered for the engine when the store runs on a
/// physical in-process connection (SQLite or a Postgres pool), sent as-is
/// over the canonical transport otherwise.
async fn default_instance_id(store: &LibraryStore, plugin_key: &str) -> Result<Option<String>> {
    const SQL: &str =
        "SELECT plugin_instance_id FROM plugin_instance_defaults WHERE plugin_key = ?";
    let values = [sea_orm::Value::from(plugin_key)];
    let type_env = crate::migrations::host_sql_type_env();
    if store.leftover_in_process() {
        let rows =
            crate::sql_plan::query_sql_on(true, store.db(), SQL, values, &type_env, 1).await?;
        return rows
            .first()
            .map(|row| super::batch::text_cell(row, 0))
            .transpose();
    }
    let rows = bookclerk_db_exec::query_canonical_stamped(store.db(), SQL, values, &type_env)
        .await
        .map_err(LibraryError::Orm)?;
    match rows.first() {
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
///
/// SQLite reports `SQLITE_BUSY` and `SQLITE_BUSY_SNAPSHOT` from a second
/// connection at insert or commit. Both contain `busy`.
fn enrollment_contention(err: &LibraryError) -> bool {
    let text = err.to_string();
    if bookclerk_db_exec::classify_db_err_message(&text) != bookclerk_db_exec::DbErrorClass::Other {
        return true;
    }
    let text = text.to_ascii_lowercase();
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
        display_name: row.display_name,
        created_at: row.created_at,
        created_by: row.created_by,
        owner_node_id: row.owner_node_id,
    }
}

/// Operator and bootstrap may mint an instance. Portal roles may not.
fn authorize_create(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } | ConfigActor::Bootstrap => Ok(()),
        ConfigActor::Scheduler
        | ConfigActor::Administrator { .. }
        | ConfigActor::Member { .. }
        | ConfigActor::NodeOwner(_)
        | ConfigActor::ClusterOwner(_) => Err(LibraryError::Forbidden(
            "unauthorized configuration write: operator authority is required".into(),
        )),
    }
}

/// Rejects an empty or oversized plugin key before insert.
fn validate_display_name(name: &str) -> Result<()> {
    if name.len() > 64 || name.chars().any(char::is_control) {
        return Err(LibraryError::Invalid("invalid plugin instance: display_name must be at most 64 bytes without control characters".into()));
    }
    Ok(())
}

/// Rejects an empty or oversized plugin key.
fn validate_plugin_key(plugin_key: &str) -> Result<()> {
    if plugin_key.is_empty() || plugin_key.len() > 512 || plugin_key.contains('\u{0000}') {
        return Err(LibraryError::Invalid(
            "invalid plugin instance: plugin_key must be 1..=512 characters without NUL".into(),
        ));
    }
    Ok(())
}
