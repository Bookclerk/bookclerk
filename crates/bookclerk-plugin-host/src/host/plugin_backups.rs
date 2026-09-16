//! Canonical backup capture/restore for registered plugin database bindings.
//!
//! Bindings are enumerated from `plugin_databases`, opened through the active
//! adapter session, and exported with the shared canonical engine. Restore
//! provisions the **target** adapter's unit (not the capture `unit_ref`) and
//! does not run plugin-owned migrations.

use bookclerk_config::{Config, DatabasePluginKind};
use bookclerk_library::{
    restore_backup_unit, BackupRepository, BackupUnit, CanonicalRestoreKind, CanonicalRestoreOpts,
    LibraryStore, PluginDatabaseRecord, PreparedPluginUnit,
};
use sea_orm::DatabaseConnection;

use super::database::{backup_adapter_id, plugin_binding_unit_ref, ExternalDatabase};
use crate::{PluginError, Result};

/// Exports every registered plugin-owned database binding as SeaORM sessions.
///
/// A missing `plugin_databases` table (uninitialized library) is treated as an
/// empty registry. Fails closed if any registered binding cannot be opened.
///
/// # Errors
///
/// Returns when a binding is inaccessible.
pub async fn export_registered_plugin_units(
    ext: &ExternalDatabase,
    config: &Config,
    library: &DatabaseConnection,
) -> Result<Vec<PreparedPluginUnit>> {
    let registered = match LibraryStore::from_connection(library.clone())
        .list_plugin_databases(None)
        .await
    {
        Ok(rows) => rows,
        Err(err) if registry_missing(&err.to_string()) => return Ok(Vec::new()),
        Err(err) => return Err(PluginError::message(err.to_string())),
    };
    let backend = library.get_database_backend();
    let backend_at_capture = backup_adapter_id(&config.database.plugin);
    let mut out = Vec::with_capacity(registered.len());
    for rec in registered {
        out.push(export_one_plugin_unit(ext, config, &rec, backend, &backend_at_capture).await?);
    }
    Ok(out)
}

/// Open one registered binding through the active adapter (lookup-only).
async fn export_one_plugin_unit(
    ext: &ExternalDatabase,
    config: &Config,
    rec: &PluginDatabaseRecord,
    backend: sea_orm::DbBackend,
    backend_at_capture: &str,
) -> Result<PreparedPluginUnit> {
    let (db, caps) = ext
        .open_binding_seaorm(
            config,
            &rec.plugin_id,
            &rec.binding,
            &rec.unit_ref,
            false,
            backend,
        )
        .await?;
    if !caps.supports_consistent_backup_read() {
        return Err(PluginError::message(format!(
            "plugin database `{}/{}` adapter does not advertise consistentBackupRead",
            rec.plugin_id, rec.binding
        )));
    }
    Ok(PreparedPluginUnit {
        plugin_id: rec.plugin_id.clone(),
        binding: rec.binding.clone(),
        backend_at_capture: backend_at_capture.to_string(),
        db,
        max_result_rows: caps.max_result_rows,
        max_result_bytes: caps.max_result_bytes,
        max_atomic_result_bytes: caps.max_atomic_result_bytes,
    })
}

/// True when `msg` indicates the plugin-database registry table is missing.
fn registry_missing(msg: &str) -> bool {
    let msg = msg.to_ascii_lowercase();
    msg.contains("plugin_databases")
        && (msg.contains("no such table")
            || msg.contains("does not exist")
            || msg.contains("no such relation"))
}

/// Restores plugin binding units onto the **current** adapter.
///
/// Each unit is replaced independently. A later failure leaves earlier units
/// already replaced (no cross-database atomicity). Plugin migrations are not
/// run; version rows restore as captured data. Registry rows are rebound to
/// the target adapter's physical placement.
///
/// Each `open_binding_seaorm` session must itself advertise `atomicUnitRestore`.
/// Restore binds, payload, and request limits come from that session.
///
/// # Errors
///
/// Returns when provisioning or canonical restore fails. Partial completion is
/// reported in the error message.
pub async fn restore_plugin_backup_units(
    ext: &ExternalDatabase,
    config: &Config,
    library: &DatabaseConnection,
    repo: &BackupRepository,
    units: &[BackupUnit],
) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    let kind = DatabasePluginKind::parse(&config.database.plugin);
    let backend_kind = backup_adapter_id(&config.database.plugin);
    let backend = library.get_database_backend();
    let store = LibraryStore::from_connection(library.clone());
    for (restored, prepared) in units.iter().enumerate() {
        let plugin_id = prepared.plugin_id.as_deref().ok_or_else(|| {
            PluginError::message("plugin backup unit is missing plugin_id".to_string())
        })?;
        let binding = prepared.binding.as_deref().ok_or_else(|| {
            PluginError::message("plugin backup unit is missing binding".to_string())
        })?;
        let unit_ref = plugin_binding_unit_ref(config, kind, plugin_id, binding);
        let (db, caps) = ext
            .open_binding_seaorm(config, plugin_id, binding, &unit_ref, true, backend)
            .await
            .map_err(|err| {
                PluginError::message(format!(
                    "plugin restore failed after {restored} unit(s); opening `{plugin_id}/{binding}`: {err}"
                ))
            })?;
        if !caps.supports_atomic_unit_restore() {
            return Err(PluginError::message(format!(
                "plugin restore failed after {restored} unit(s); `{plugin_id}/{binding}` \
                 adapter does not advertise atomicUnitRestore"
            )));
        }
        let opts = CanonicalRestoreOpts::from_caps(&caps).map_err(|err| {
            PluginError::message(format!(
                "plugin restore failed after {restored} unit(s); `{plugin_id}/{binding}` \
                 advertised schema flags are not a known versioning contract: {err}"
            ))
        })?;
        restore_backup_unit(
            &db,
            repo,
            prepared,
            CanonicalRestoreKind::PluginBinding,
            &opts,
            false,
        )
        .await
        .map_err(|err| {
            PluginError::message(format!(
                "plugin restore failed after {restored} unit(s); replacing `{plugin_id}/{binding}`: {err}"
            ))
        })?;
        store
            .rebind_plugin_database(plugin_id, binding, &backend_kind, &unit_ref)
            .await
            .map_err(|err| PluginError::message(err.to_string()))?;
    }
    Ok(())
}
