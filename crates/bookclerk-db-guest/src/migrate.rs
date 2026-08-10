//! Apply greenfield DDL from `bookclerk-library` (D1 / Postgres).

use bookclerk_library::{migrations, LibraryError, Result};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, Value};

/// Apply pending schema versions to backends without `rusqlite_migration`.
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

    let steps: &[&str] = if backend == DbBackend::Postgres {
        migrations::migration_sql_postgres()
    } else {
        migrations::migration_sql()
    };

    let insert = if backend == DbBackend::Postgres {
        "INSERT INTO schema_migrations (version) VALUES ($1)"
    } else {
        "INSERT INTO schema_migrations (version) VALUES (?)"
    };

    for (idx, schema) in steps.iter().enumerate() {
        let version = (idx + 1) as i64;
        if applied.contains(&version) {
            continue;
        }
        for stmt in split_sql_statements(schema) {
            db.execute_raw(Statement::from_string(backend, stmt))
                .await
                .map_err(LibraryError::Orm)?;
        }
        db.execute_raw(Statement::from_sql_and_values(
            backend,
            insert,
            [Value::from(version)],
        ))
        .await
        .map_err(LibraryError::Orm)?;
    }
    Ok(())
}

/// Typed SQL `NULL` for proxy columns so SeaORM `Option<T>` decoding works.
#[must_use]
pub fn typed_null(decl_type: Option<&str>, column: &str) -> Value {
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

fn null_kind_for_column(column: &str) -> Value {
    const INTEGER_COLUMNS: &[&str] = &[
        "id",
        "identity_id",
        "title_request_id",
        "scan_enabled",
        "is_finished",
        "is_abridged",
        "length_minutes",
        "price_cents",
        "list_price_cents",
        "member_price_cents",
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

fn split_sql_statements(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
