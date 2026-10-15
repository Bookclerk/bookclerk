//! Pinned plugin settings schemas (`plugin_schemas`).
//!
//! A node that discovers or installs a plugin pins the manifest's
//! `[[settings]]` schema here together with the package version that
//! produced it. Instance documents are validated against the pinned row, so
//! an API node that does not have the guest installed can still validate a
//! write. Versions order the pin: a newer local package re-pins (and the
//! caller reconciles documents to the new schema), an equal version with a
//! different schema re-pins in place, and an older local package is
//! [`SchemaPinChange::Stale`] and must not spawn. Equality is the
//! fingerprint of the normalized schema, not the manifest bytes, so line
//! endings or comments never flip the pin. Every write is a compare-and-swap
//! on the row that was read, so two nodes pinning at once cannot leave an
//! older version on top of a newer one. The row records which node wrote it.

use std::cmp::Ordering;

use bookclerk_plugin_abi::package_version::{package_version_order, parse_package_version};
use bookclerk_plugin_abi::{SettingsSchema, SETTINGS_SCHEMA_VERSION};
use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use sha2::{Digest, Sha256};

use crate::entities::plugin_schemas;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// One `plugin_schemas` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPluginSchema {
    /// Canonical plugin key.
    pub plugin_key: String,
    /// Manifest id (display alias) at pin time.
    pub manifest_id: String,
    /// Fingerprint of the normalized schema ([`schema_fingerprint`]).
    pub schema_sha256: String,
    /// Semver package version that produced the pinned schema.
    pub package_version: String,
    /// Declared settings.
    pub schema: SettingsSchema,
    /// RFC 3339 time of the last pin.
    pub recorded_at: String,
    /// Node that recorded the last pin.
    pub recorded_by_node: String,
}

/// Inputs for [`pin_plugin_schema`].
#[derive(Debug, Clone)]
pub struct SchemaPin<'a> {
    /// Canonical plugin key.
    pub plugin_key: &'a str,
    /// Manifest id (display alias).
    pub manifest_id: &'a str,
    /// Semver package version of the local manifest.
    pub package_version: &'a str,
    /// Declared settings.
    pub schema: &'a SettingsSchema,
    /// Node recording the pin.
    pub node_id: &'a str,
}

/// How [`pin_plugin_schema`] changed the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaPinChange {
    /// The row already matched the local manifest and version.
    Unchanged,
    /// No node had pinned this plugin before.
    Pinned,
    /// The local package is newer than the pin; the row now carries it.
    /// Callers prune instance documents to the new schema.
    Upgraded {
        /// Version the row carried before this pin.
        previous_version: String,
    },
    /// Same version, different manifest (an unreleased edit); re-pinned in
    /// place. Callers prune instance documents as for an upgrade.
    Repinned,
    /// The local package is older than the pin. The row was not touched and
    /// the plugin must not be spawned by this node.
    Stale {
        /// The local manifest's version.
        local_version: String,
    },
    /// The active release moved to an older version (a rollback); the row
    /// follows it. Callers prune instance documents as for an upgrade.
    RolledBack {
        /// Version the row carried before this pin.
        previous_version: String,
    },
}

/// Outcome of [`pin_plugin_schema`]: the row that is now pinned and how it
/// relates to the local manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaPinOutcome {
    /// The pinned row. For [`SchemaPinChange::Stale`] this is the newer pin
    /// another node wrote, not the local manifest.
    pub schema: PinnedPluginSchema,
    /// What happened to the row.
    pub change: SchemaPinChange,
}

/// Compare-and-swap attempts before giving up on a contended row.
const PIN_ATTEMPTS: usize = 4;

/// Hex SHA-256 of the schema's canonical JSON. Two manifests that declare
/// the same settings fingerprint the same, whatever their bytes.
///
/// # Errors
///
/// Returns an error when the schema fails to serialize.
pub fn schema_fingerprint(schema: &SettingsSchema) -> Result<String> {
    let json = serde_json::to_string(schema)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("encode settings schema: {err}")))?;
    Ok(hex::encode(Sha256::digest(json.as_bytes())))
}

/// Pins `pin` according to the version rule in the module docs.
///
/// An unchanged row is left alone, including `recorded_at`. The insert and
/// the update are both conditional on the row read just before, and a lost
/// race re-reads and decides again, so a concurrent newer pin is never
/// overwritten by an older one.
///
/// # Errors
///
/// Returns an error when `package_version` is not semver, the schema fails
/// to serialize, the write fails, or the row keeps changing underneath.
pub async fn pin_plugin_schema(
    store: &LibraryStore,
    pin: &SchemaPin<'_>,
) -> Result<SchemaPinOutcome> {
    pin_schema(store, pin, false).await
}

/// Pins the schema of the plugin's active release.
///
/// The pinned schema of a released plugin is the active release's schema,
/// so an older version is pinned too ([`SchemaPinChange::RolledBack`])
/// instead of being refused as stale.
///
/// # Errors
///
/// As [`pin_plugin_schema`].
pub async fn pin_version_schema(
    store: &LibraryStore,
    pin: &SchemaPin<'_>,
) -> Result<SchemaPinOutcome> {
    pin_schema(store, pin, true).await
}

/// Shared compare-and-swap loop; `follow_release` pins an older version.
async fn pin_schema(
    store: &LibraryStore,
    pin: &SchemaPin<'_>,
    follow_release: bool,
) -> Result<SchemaPinOutcome> {
    let local_version = parse_package_version(pin.package_version)
        .map_err(|detail| LibraryError::Schema(format!("plugin `{}`: {detail}", pin.plugin_key)))?
        .to_string();
    let fingerprint = schema_fingerprint(pin.schema)?;
    let settings_json = serde_json::to_string(pin.schema)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("encode settings schema: {err}")))?;
    for _ in 0..PIN_ATTEMPTS {
        let existing = load_plugin_schema(store, pin.plugin_key).await?;
        let Some(existing) = existing else {
            let model = plugin_schemas::ActiveModel {
                plugin_key: Set(pin.plugin_key.to_string()),
                manifest_id: Set(pin.manifest_id.to_string()),
                schema_sha256: Set(fingerprint.clone()),
                package_version: Set(local_version.clone()),
                schema_version: Set(i64::from(SETTINGS_SCHEMA_VERSION)),
                settings_json: Set(settings_json.clone()),
                recorded_at: Set(Utc::now().to_rfc3339()),
                recorded_by_node: Set(pin.node_id.to_string()),
            };
            let inserted = plugin_schemas::Entity::insert(model)
                .on_conflict(
                    OnConflict::column(plugin_schemas::Column::PluginKey)
                        .do_nothing()
                        .to_owned(),
                )
                .exec_without_returning(store.db())
                .await
                .map_err(LibraryError::Orm)?;
            if inserted == 0 {
                continue; // another node pinned first; decide against its row
            }
            let schema = require_plugin_schema(store, pin.plugin_key).await?;
            return Ok(SchemaPinOutcome {
                schema,
                change: SchemaPinChange::Pinned,
            });
        };
        let order =
            package_version_order(&local_version, &existing.package_version).map_err(|detail| {
                LibraryError::Schema(format!("plugin `{}`: {detail}", pin.plugin_key))
            })?;
        let change = match order {
            Ordering::Less if follow_release => SchemaPinChange::RolledBack {
                previous_version: existing.package_version.clone(),
            },
            Ordering::Less => {
                return Ok(SchemaPinOutcome {
                    schema: existing,
                    change: SchemaPinChange::Stale { local_version },
                });
            }
            Ordering::Greater => SchemaPinChange::Upgraded {
                previous_version: existing.package_version.clone(),
            },
            Ordering::Equal => {
                if existing.schema_sha256 == fingerprint
                    && existing.manifest_id == pin.manifest_id
                    && existing.package_version == local_version
                {
                    return Ok(SchemaPinOutcome {
                        schema: existing,
                        change: SchemaPinChange::Unchanged,
                    });
                }
                SchemaPinChange::Repinned
            }
        };
        let update = plugin_schemas::ActiveModel {
            plugin_key: sea_orm::ActiveValue::NotSet,
            manifest_id: Set(pin.manifest_id.to_string()),
            schema_sha256: Set(fingerprint.clone()),
            package_version: Set(local_version.clone()),
            schema_version: Set(i64::from(SETTINGS_SCHEMA_VERSION)),
            settings_json: Set(settings_json.clone()),
            recorded_at: Set(Utc::now().to_rfc3339()),
            recorded_by_node: Set(pin.node_id.to_string()),
        };
        // Compare-and-swap on what was read: a row another node moved in the
        // meantime is not touched, and the loop decides against it instead.
        let updated = plugin_schemas::Entity::update_many()
            .set(update)
            .filter(plugin_schemas::Column::PluginKey.eq(pin.plugin_key))
            .filter(plugin_schemas::Column::PackageVersion.eq(existing.package_version.as_str()))
            .filter(plugin_schemas::Column::SchemaSha256.eq(existing.schema_sha256.as_str()))
            .filter(plugin_schemas::Column::RecordedAt.eq(existing.recorded_at.as_str()))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if updated.rows_affected == 0 {
            continue;
        }
        let schema = require_plugin_schema(store, pin.plugin_key).await?;
        return Ok(SchemaPinOutcome { schema, change });
    }
    Err(LibraryError::Conflict(format!(
        "plugin schema for `{}` kept changing while pinning; retry",
        pin.plugin_key
    )))
}

/// Whether the pinned row for `plugin_key` is newer than `local_version`.
///
/// `None` when nothing is pinned yet. Used by the reconciler before a spawn
/// so a node that was not the one to upgrade the pin still refuses to run an
/// older package against documents written for the newer schema.
///
/// # Errors
///
/// Returns an error when the read fails or `local_version` is not semver.
pub async fn stale_against_pin(
    store: &LibraryStore,
    plugin_key: &str,
    local_version: &str,
) -> Result<Option<PinnedPluginSchema>> {
    let Some(pinned) = load_plugin_schema(store, plugin_key).await? else {
        return Ok(None);
    };
    let order = package_version_order(local_version, &pinned.package_version)
        .map_err(|detail| LibraryError::Schema(format!("plugin `{plugin_key}`: {detail}")))?;
    Ok((order == Ordering::Less).then_some(pinned))
}

/// The pinned schema for `plugin_key`, if any node has pinned one.
///
/// # Errors
///
/// Returns an error when the read fails or the stored JSON is not a schema
/// this binary understands.
pub async fn load_plugin_schema(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<Option<PinnedPluginSchema>> {
    let row = plugin_schemas::Entity::find_by_id(plugin_key.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    row.map(schema_from_model).transpose()
}

/// Every pinned schema, ordered by plugin key.
///
/// # Errors
///
/// Returns an error when the read fails or a row cannot be decoded.
pub async fn list_plugin_schemas(store: &LibraryStore) -> Result<Vec<PinnedPluginSchema>> {
    let rows = plugin_schemas::Entity::find()
        .order_by_asc(plugin_schemas::Column::PluginKey)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(schema_from_model).collect()
}

/// Loads the pinned schema or fails closed with an operator-facing message.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] when no node has pinned the plugin yet.
pub async fn require_plugin_schema(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<PinnedPluginSchema> {
    load_plugin_schema(store, plugin_key).await?.ok_or_else(|| {
        LibraryError::NotFound(format!(
            "plugin `{plugin_key}` has no pinned settings schema; install it on a node first"
        ))
    })
}

/// Decodes one `plugin_schemas` row, rejecting a schema version this binary does not apply.
fn schema_from_model(row: plugin_schemas::Model) -> Result<PinnedPluginSchema> {
    if row.schema_version != i64::from(SETTINGS_SCHEMA_VERSION) {
        return Err(LibraryError::Schema(format!(
            "plugin schema for `{}` uses settings schema version {}; this binary applies {}",
            row.plugin_key, row.schema_version, SETTINGS_SCHEMA_VERSION
        )));
    }
    let schema: SettingsSchema = serde_json::from_str(&row.settings_json).map_err(|err| {
        LibraryError::Schema(format!(
            "plugin schema for `{}` is not decodable: {err}",
            row.plugin_key
        ))
    })?;
    Ok(PinnedPluginSchema {
        plugin_key: row.plugin_key,
        manifest_id: row.manifest_id,
        schema_sha256: row.schema_sha256,
        package_version: row.package_version,
        schema,
        recorded_at: row.recorded_at,
        recorded_by_node: row.recorded_by_node,
    })
}
