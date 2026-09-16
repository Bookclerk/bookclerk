//! Registry of provisioned isolated plugin database bindings.
//!
//! One `plugin_databases` row per `(plugin_id, binding)`: `backend_kind` is
//! the adapter family that provisioned the unit and `unit_ref` the
//! backend-native unit (file path, schema name, or D1 database name).

use super::{now_str, LibraryStore};
use crate::error::{LibraryError, Result};

/// One provisioned plugin database binding registry row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDatabaseRecord {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Binding name from `plugin.toml` `capabilities.bindings.databases`.
    pub binding: String,
    /// Adapter family that provisioned the unit (`sqlite`, `postgres`, `d1`).
    pub backend_kind: String,
    /// Backend-native unit: file path, schema name, or D1 database name.
    pub unit_ref: String,
    /// RFC 3339 provisioning time.
    pub created_at: String,
}

impl LibraryStore {
    /// Records one provisioned binding unit, keeping any existing row.
    ///
    /// Returns the authoritative row (an earlier registration wins so a
    /// re-open never silently re-targets a binding at a different unit).
    ///
    /// # Errors
    ///
    /// Returns an error when the insert or lookup fails.
    pub async fn record_plugin_database(
        &self,
        plugin_id: &str,
        binding: &str,
        backend_kind: &str,
        unit_ref: &str,
    ) -> Result<PluginDatabaseRecord> {
        if let Some(existing) = self.get_plugin_database(plugin_id, binding).await? {
            return Ok(existing);
        }
        // Valid on SQLite (3.24+), PostgreSQL, and D1 alike.
        let conflict = "ON CONFLICT (plugin_id, binding) DO NOTHING";
        bookclerk_db_exec::execute_canonical(
            self.db(),
            &format!(
                "INSERT INTO plugin_databases \
                 (plugin_id, binding, backend_kind, unit_ref, created_at) \
                 VALUES (?, ?, ?, ?, ?) {conflict}"
            ),
            [
                plugin_id.into(),
                binding.into(),
                backend_kind.into(),
                unit_ref.into(),
                now_str().into(),
            ],
        )
        .await
        .map_err(LibraryError::Orm)?;
        self.get_plugin_database(plugin_id, binding)
            .await?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "plugin database registry row missing after insert"
                ))
            })
    }

    /// The registry row for `(plugin_id, binding)`, if provisioned.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn get_plugin_database(
        &self,
        plugin_id: &str,
        binding: &str,
    ) -> Result<Option<PluginDatabaseRecord>> {
        let rows = bookclerk_db_exec::query_canonical(
            self.db(),
            "SELECT plugin_id, binding, backend_kind, unit_ref, created_at \
             FROM plugin_databases WHERE plugin_id = ? AND binding = ?",
            [plugin_id.into(), binding.into()],
        )
        .await
        .map_err(LibraryError::Orm)?;
        rows.first().map(row_to_record).transpose()
    }

    /// All registry rows, optionally filtered to one plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn list_plugin_databases(
        &self,
        plugin_id: Option<&str>,
    ) -> Result<Vec<PluginDatabaseRecord>> {
        let rows = match plugin_id {
            Some(id) => {
                bookclerk_db_exec::query_canonical(
                    self.db(),
                    "SELECT plugin_id, binding, backend_kind, unit_ref, created_at \
                     FROM plugin_databases WHERE plugin_id = ? ORDER BY plugin_id, binding",
                    [id.into()],
                )
                .await
            }
            None => {
                bookclerk_db_exec::query_canonical(
                    self.db(),
                    "SELECT plugin_id, binding, backend_kind, unit_ref, created_at \
                     FROM plugin_databases ORDER BY plugin_id, binding",
                    [],
                )
                .await
            }
        }
        .map_err(LibraryError::Orm)?;
        rows.iter().map(row_to_record).collect()
    }

    /// Deletes registry rows for a plugin (one binding, or all of them).
    ///
    /// Removes only the registry entry; dropping the underlying unit is the
    /// caller's responsibility (operator CLI / uninstall flow).
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn remove_plugin_databases(
        &self,
        plugin_id: &str,
        binding: Option<&str>,
    ) -> Result<u64> {
        let res = match binding {
            Some(binding) => {
                bookclerk_db_exec::execute_canonical(
                    self.db(),
                    "DELETE FROM plugin_databases WHERE plugin_id = ? AND binding = ?",
                    [plugin_id.into(), binding.into()],
                )
                .await
            }
            None => {
                bookclerk_db_exec::execute_canonical(
                    self.db(),
                    "DELETE FROM plugin_databases WHERE plugin_id = ?",
                    [plugin_id.into()],
                )
                .await
            }
        }
        .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected())
    }

    /// Inserts or updates placement for one `(plugin_id, binding)` on restore.
    ///
    /// Logical identity is portable; `backend_kind` / `unit_ref` are the
    /// **target** adapter's physical placement, never the source's.
    ///
    /// # Errors
    ///
    /// Returns when the upsert fails.
    pub async fn rebind_plugin_database(
        &self,
        plugin_id: &str,
        binding: &str,
        backend_kind: &str,
        unit_ref: &str,
    ) -> Result<PluginDatabaseRecord> {
        let now = now_str();
        bookclerk_db_exec::execute_canonical(
            self.db(),
            "UPDATE plugin_databases SET backend_kind = ?, unit_ref = ? \
             WHERE plugin_id = ? AND binding = ?",
            [
                backend_kind.into(),
                unit_ref.into(),
                plugin_id.into(),
                binding.into(),
            ],
        )
        .await
        .map_err(LibraryError::Orm)?;
        if let Some(existing) = self.get_plugin_database(plugin_id, binding).await? {
            return Ok(existing);
        }
        bookclerk_db_exec::execute_canonical(
            self.db(),
            "INSERT INTO plugin_databases \
             (plugin_id, binding, backend_kind, unit_ref, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            [
                plugin_id.into(),
                binding.into(),
                backend_kind.into(),
                unit_ref.into(),
                now.into(),
            ],
        )
        .await
        .map_err(LibraryError::Orm)?;
        self.get_plugin_database(plugin_id, binding)
            .await?
            .ok_or_else(|| {
                LibraryError::Schema(format!(
                    "plugin database `{plugin_id}/{binding}` was not recorded after rebind"
                ))
            })
    }
}

/// Decodes one registry row.
fn row_to_record(row: &sea_orm::QueryResult) -> Result<PluginDatabaseRecord> {
    Ok(PluginDatabaseRecord {
        plugin_id: row.try_get("", "plugin_id").map_err(LibraryError::Orm)?,
        binding: row.try_get("", "binding").map_err(LibraryError::Orm)?,
        backend_kind: row.try_get("", "backend_kind").map_err(LibraryError::Orm)?,
        unit_ref: row.try_get("", "unit_ref").map_err(LibraryError::Orm)?,
        created_at: row.try_get("", "created_at").map_err(LibraryError::Orm)?,
    })
}
