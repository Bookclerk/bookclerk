//! Pinned plugin settings schemas (`plugin_schemas`).
//!
//! A host that discovers or installs a plugin pins the manifest's
//! `[[settings]]` schema here together with the package version that
//! produced it. Instance documents are validated against the pinned row, so
//! an API host that does not have the guest installed can still validate a
//! write. Versions order the pin: a newer local package re-pins (and the
//! caller prunes documents to the new schema), an equal version with a
//! changed manifest re-pins in place, and an older local package is
//! [`SchemaPinChange::Stale`] and must not spawn. The row records which host
//! wrote it.

use std::cmp::Ordering;

use bookclerk_plugin_abi::package_version::{package_version_order, parse_package_version};
use bookclerk_plugin_abi::{SettingsSchema, SETTINGS_SCHEMA_VERSION};
use chrono::Utc;
use sea_orm::{ActiveValue::Set, EntityTrait, QueryOrder};

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
    /// SHA-256 of the manifest the schema came from.
    pub manifest_sha256: String,
    /// Semver package version that produced the pinned schema.
    pub package_version: String,
    /// Declared settings.
    pub schema: SettingsSchema,
    /// RFC 3339 time of the last pin.
    pub recorded_at: String,
    /// Host that recorded the last pin.
    pub recorded_by_host: String,
}

/// Inputs for [`pin_plugin_schema`].
#[derive(Debug, Clone)]
pub struct SchemaPin<'a> {
    /// Canonical plugin key.
    pub plugin_key: &'a str,
    /// Manifest id (display alias).
    pub manifest_id: &'a str,
    /// SHA-256 of the manifest file.
    pub manifest_sha256: &'a str,
    /// Semver package version of the local manifest.
    pub package_version: &'a str,
    /// Declared settings.
    pub schema: &'a SettingsSchema,
    /// Host recording the pin.
    pub host_id: &'a str,
}

/// How [`pin_plugin_schema`] changed the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaPinChange {
    /// The row already matched the local manifest and version.
    Unchanged,
    /// No host had pinned this plugin before.
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
    /// the plugin must not be spawned by this host.
    Stale {
        /// The local manifest's version.
        local_version: String,
    },
}

/// Outcome of [`pin_plugin_schema`]: the row that is now pinned and how it
/// relates to the local manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaPinOutcome {
    /// The pinned row. For [`SchemaPinChange::Stale`] this is the newer pin
    /// another host wrote, not the local manifest.
    pub schema: PinnedPluginSchema,
    /// What happened to the row.
    pub change: SchemaPinChange,
}

/// Pins `pin` according to the version rule in the module docs.
///
/// An unchanged row is left alone, including `recorded_at`.
///
/// # Errors
///
/// Returns an error when `package_version` is not semver, the schema fails
/// to serialize, or the write fails.
pub async fn pin_plugin_schema(
    store: &LibraryStore,
    pin: &SchemaPin<'_>,
) -> Result<SchemaPinOutcome> {
    let local_version = parse_package_version(pin.package_version)
        .map_err(|detail| LibraryError::Schema(format!("plugin `{}`: {detail}", pin.plugin_key)))?
        .to_string();
    let change = match load_plugin_schema(store, pin.plugin_key).await? {
        None => SchemaPinChange::Pinned,
        Some(existing) => {
            let order = package_version_order(&local_version, &existing.package_version).map_err(
                |detail| LibraryError::Schema(format!("plugin `{}`: {detail}", pin.plugin_key)),
            )?;
            match order {
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
                    if existing.manifest_sha256 == pin.manifest_sha256
                        && existing.schema == *pin.schema
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
            }
        }
    };
    let settings_json = serde_json::to_string(pin.schema)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("encode settings schema: {err}")))?;
    let now = Utc::now().to_rfc3339();
    let model = plugin_schemas::ActiveModel {
        plugin_key: Set(pin.plugin_key.to_string()),
        manifest_id: Set(pin.manifest_id.to_string()),
        manifest_sha256: Set(pin.manifest_sha256.to_string()),
        package_version: Set(local_version),
        schema_version: Set(i64::from(SETTINGS_SCHEMA_VERSION)),
        settings_json: Set(settings_json),
        recorded_at: Set(now),
        recorded_by_host: Set(pin.host_id.to_string()),
    };
    plugin_schemas::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(plugin_schemas::Column::PluginKey)
                .update_columns([
                    plugin_schemas::Column::ManifestId,
                    plugin_schemas::Column::ManifestSha256,
                    plugin_schemas::Column::PackageVersion,
                    plugin_schemas::Column::SchemaVersion,
                    plugin_schemas::Column::SettingsJson,
                    plugin_schemas::Column::RecordedAt,
                    plugin_schemas::Column::RecordedByHost,
                ])
                .to_owned(),
        )
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let schema = load_plugin_schema(store, pin.plugin_key)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin schema {}", pin.plugin_key)))?;
    Ok(SchemaPinOutcome { schema, change })
}

/// Whether the pinned row for `plugin_key` is newer than `local_version`.
///
/// `None` when nothing is pinned yet. Used by the reconciler before a spawn
/// so a host that was not the one to upgrade the pin still refuses to run an
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

/// The pinned schema for `plugin_key`, if any host has pinned one.
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
/// Returns [`LibraryError::NotFound`] when no host has pinned the plugin yet.
pub async fn require_plugin_schema(
    store: &LibraryStore,
    plugin_key: &str,
) -> Result<PinnedPluginSchema> {
    load_plugin_schema(store, plugin_key).await?.ok_or_else(|| {
        LibraryError::NotFound(format!(
            "plugin `{plugin_key}` has no pinned settings schema; install it on a host first"
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
        manifest_sha256: row.manifest_sha256,
        package_version: row.package_version,
        schema,
        recorded_at: row.recorded_at,
        recorded_by_host: row.recorded_by_host,
    })
}
