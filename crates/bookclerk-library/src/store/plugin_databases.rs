//! Registry of provisioned isolated plugin database bindings.
//!
//! One `plugin_databases` row per `(plugin_id, binding)`: `backend_kind` is
//! the adapter family that provisioned the unit and `unit_ref` the
//! backend-native unit (file path, schema name, or D1 database name).

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, Value};

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

/// Rewrite SQLite `?` placeholders to Postgres `$1`…`$n` (sqlx does not).
fn stmt_for(
    backend: DatabaseBackend,
    sql: &str,
    values: impl IntoIterator<Item = Value>,
) -> Statement {
    let sql = if backend == DatabaseBackend::Postgres {
        let mut n = 0u32;
        let mut out = String::with_capacity(sql.len() + 16);
        for ch in sql.chars() {
            if ch == '?' {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        sql.to_string()
    };
    Statement::from_sql_and_values(backend, sql, values)
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
        let backend = self.db().get_database_backend();
        if let Some(existing) = self.get_plugin_database(plugin_id, binding).await? {
            return Ok(existing);
        }
        // Valid on SQLite (3.24+), PostgreSQL, and D1 alike.
        let conflict = "ON CONFLICT (plugin_id, binding) DO NOTHING";
        self.db()
            .execute_raw(stmt_for(
                backend,
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
            ))
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
        let backend = self.db().get_database_backend();
        let rows = self
            .db()
            .query_all_raw(stmt_for(
                backend,
                "SELECT plugin_id, binding, backend_kind, unit_ref, created_at \
                 FROM plugin_databases WHERE plugin_id = ? AND binding = ?",
                [plugin_id.into(), binding.into()],
            ))
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
        let backend = self.db().get_database_backend();
        let stmt = match plugin_id {
            Some(id) => stmt_for(
                backend,
                "SELECT plugin_id, binding, backend_kind, unit_ref, created_at \
                 FROM plugin_databases WHERE plugin_id = ? ORDER BY plugin_id, binding",
                [id.into()],
            ),
            None => stmt_for(
                backend,
                "SELECT plugin_id, binding, backend_kind, unit_ref, created_at \
                 FROM plugin_databases ORDER BY plugin_id, binding",
                [],
            ),
        };
        let rows = self
            .db()
            .query_all_raw(stmt)
            .await
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
        let backend = self.db().get_database_backend();
        let stmt = match binding {
            Some(binding) => stmt_for(
                backend,
                "DELETE FROM plugin_databases WHERE plugin_id = ? AND binding = ?",
                [plugin_id.into(), binding.into()],
            ),
            None => stmt_for(
                backend,
                "DELETE FROM plugin_databases WHERE plugin_id = ?",
                [plugin_id.into()],
            ),
        };
        let res = self
            .db()
            .execute_raw(stmt)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected())
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
