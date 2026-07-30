//! Database plugin connections (SeaORM).
//!
//! Built-in backends:
//! - `sqlite` — local file through rusqlite (proxy; default)
//! - `d1` — Cloudflare D1 over the HTTP API (proxy)
//! - `postgres` — PostgreSQL via sqlx-postgres (native sqlx connection)
//!
//! SeaORM is the query layer for [`crate::LibraryStore`]: every backend is a
//! [`DatabaseConnection`] proxy or native connection, and the store issues SQL
//! through [`ConnectionTrait`]. Local SQLite files migrate with
//! `rusqlite_migration` (`PRAGMA user_version`); D1 and Postgres apply the
//! same migration texts via [`apply_pending_migrations`].

mod d1;
mod postgres;
mod sqlite;

use std::path::Path;
use std::sync::Arc;

use bookclerk_config::{Config, DatabasePluginKind};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement, Value};

use crate::error::{LibraryError, Result};
use crate::migrations;

pub use d1::{resolve_d1_api_token, D1Proxy};
pub use postgres::{connect_postgres, resolve_postgres_url};
pub use sqlite::SqliteProxy;

/// Open the configured database plugin.
pub async fn connect_from_config(config: &Config) -> Result<DatabaseConnection> {
    match config.database.active_plugin()? {
        DatabasePluginKind::Sqlite => {
            let path = config.database.sqlite_path(&config.paths().files_dir);
            connect_sqlite(&path).await
        }
        DatabasePluginKind::D1 => connect_d1(config).await,
        DatabasePluginKind::Postgres => connect_postgres(config).await,
    }
}

/// Open (or create) a local SQLite database, migrate, return a SeaORM proxy.
pub async fn connect_sqlite(path: &Path) -> Result<DatabaseConnection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = rusqlite::Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrations::migrations().to_latest(&mut conn)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn))),
    )
    .await
    .map_err(LibraryError::Orm)?;
    tracing::debug!(path = %path.display(), plugin = "sqlite", "opened library database (sea-orm proxy)");
    Ok(db)
}

/// Open an in-memory SQLite database, migrate, return a SeaORM proxy.
///
/// Same wrapping as [`connect_sqlite`] but without a backing file (tests).
pub async fn connect_sqlite_memory() -> Result<DatabaseConnection> {
    let mut conn = rusqlite::Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrations::migrations().to_latest(&mut conn)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn))),
    )
    .await
    .map_err(LibraryError::Orm)?;
    tracing::debug!(
        plugin = "sqlite",
        "opened in-memory library database (sea-orm proxy)"
    );
    Ok(db)
}

/// Open Cloudflare D1 through the HTTP API proxy.
pub async fn connect_d1(config: &Config) -> Result<DatabaseConnection> {
    let token = resolve_d1_api_token(config)?;
    let proxy = D1Proxy::new(
        config.database.d1.api_base.clone(),
        config.database.d1.account_id.clone(),
        config.database.d1.database_id.clone(),
        token,
    );
    let db = Database::connect_proxy(DbBackend::Sqlite, Arc::new(Box::new(proxy)))
        .await
        .map_err(LibraryError::Orm)?;
    db.ping().await.map_err(LibraryError::Orm)?;
    apply_pending_migrations(&db).await?;
    tracing::debug!(plugin = "d1", "opened library database (sea-orm proxy)");
    Ok(db)
}

/// Apply the greenfield schema to backends without `rusqlite_migration`.
///
/// D1 and Postgres track the schema version in a `schema_migrations` table.
/// Bookclerk is greenfield with a single schema (version 1): when the table is
/// empty, apply the backend's DDL ([`migrations::latest_schema_postgres`] for
/// Postgres, [`migrations::latest_schema_sqlite`] for D1) and record version 1.
/// Every statement uses `IF NOT EXISTS`, so re-application is a no-op.
pub async fn apply_pending_migrations(db: &DatabaseConnection) -> Result<()> {
    let backend = db.get_database_backend();
    db.execute_raw(Statement::from_string(
        backend,
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)",
    ))
    .await
    .map_err(LibraryError::Orm)?;

    let applied: std::collections::HashSet<i64> = db
        .query_all_raw(Statement::from_string(
            backend,
            "SELECT version FROM schema_migrations",
        ))
        .await
        .map_err(LibraryError::Orm)?
        .iter()
        .filter_map(|row| row.try_get::<i64>("", "version").ok())
        .collect();

    if applied.contains(&1) {
        return Ok(());
    }

    let schema = if backend == DbBackend::Postgres {
        migrations::latest_schema_postgres()
    } else {
        migrations::latest_schema_sqlite()
    };
    for stmt in split_sql_statements(schema) {
        db.execute_raw(Statement::from_string(backend, stmt))
            .await
            .map_err(LibraryError::Orm)?;
    }
    let insert = if backend == DbBackend::Postgres {
        "INSERT INTO schema_migrations (version) VALUES ($1)"
    } else {
        "INSERT INTO schema_migrations (version) VALUES (?)"
    };
    db.execute_raw(Statement::from_sql_and_values(
        backend,
        insert,
        [Value::from(1_i64)],
    ))
    .await
    .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Typed SQL `NULL` for a proxy column, so SeaORM `Option<T>` decoding works.
///
/// SeaORM decodes `Option<T>` by comparing the proxy value to `T::null()`
/// (e.g. `BigInt(None)` for `Option<i64>`), so every backend must return the
/// correctly typed empty variant instead of a blanket `String(None)`. SQLite
/// passes the column's `decl_type`; D1 (JSON, no type metadata) passes `None`
/// and relies on the column-name fallback. Column names are unambiguous across
/// the single greenfield schema (e.g. `identity_id` is always integer,
/// `rating_overall` always real, `vector` always blob).
#[must_use]
pub(crate) fn typed_null(decl_type: Option<&str>, column: &str) -> Value {
    if let Some(decl) = decl_type {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            return Value::BigInt(None);
        }
        if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            return Value::Double(None);
        }
        if d.contains("BLOB") || d.contains("BYTEA") || d.contains("BINARY") {
            return Value::Bytes(None);
        }
        if d.contains("CHAR") || d.contains("TEXT") || d.contains("CLOB") {
            return Value::String(None);
        }
    }
    null_kind_for_column(column)
}

/// Column-name null kind fallback (expression columns / D1 JSON nulls).
fn null_kind_for_column(column: &str) -> Value {
    const INTEGER_COLUMNS: &[&str] = &[
        "id",
        "identity_id",
        "scan_enabled",
        "is_finished",
        "is_abridged",
        "length_minutes",
        "dims",
        "kdf_m_cost",
        "kdf_t_cost",
        "kdf_p_cost",
    ];
    const REAL_COLUMNS: &[&str] = &[
        "rating_overall",
        "rating_performance",
        "rating_story",
        "progress",
        "current_time_seconds",
        "duration_seconds",
        "enrich_confidence",
    ];
    const BLOB_COLUMNS: &[&str] = &["vector", "ciphertext", "kdf_salt", "cipher_nonce"];

    if INTEGER_COLUMNS.contains(&column) {
        Value::BigInt(None)
    } else if REAL_COLUMNS.contains(&column) {
        Value::Double(None)
    } else if BLOB_COLUMNS.contains(&column) {
        Value::Bytes(None)
    } else {
        Value::String(None)
    }
}

/// Split a migration text into individual statements on `;`.
///
/// The migration SQL never embeds `;` inside string literals, so a simple split
/// is sufficient (and avoids relying on multi-statement support in the D1 HTTP
/// API).
fn split_sql_statements(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_sqlite_file_and_ping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let db = connect_sqlite(&path).await.unwrap();
        db.ping().await.unwrap();
        assert!(path.is_file());
    }
}
