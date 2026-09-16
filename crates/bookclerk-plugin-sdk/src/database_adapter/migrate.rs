//! Generic SQL-string helpers for database guests.
//!
//! Guests may run host-provided SQL. They must not select Bookclerk schema
//! versions or import `bookclerk_library::migrations`.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement, Value};

/// Runs each non-empty statement on `db` in order (autocommit per statement).
///
/// # Errors
///
/// Returns the engine error when a statement fails.
pub async fn execute_sql_scripts(
    db: &DatabaseConnection,
    statements: impl IntoIterator<Item = impl AsRef<str>>,
) -> std::result::Result<(), DbErr> {
    let backend = db.get_database_backend();
    for sql in statements {
        let sql = sql.as_ref().trim();
        if sql.is_empty() {
            continue;
        }
        db.execute_raw(Statement::from_string(backend, sql.to_string()))
            .await?;
    }
    Ok(())
}

/// Splits a script on top-level `;` and drops empty fragments.
///
/// Semicolons inside quoted literals, comments, and parentheses are not
/// statement boundaries (same tokenizer as host schema packs).
#[must_use]
pub fn split_sql_statements(sql: &str) -> Vec<String> {
    bookclerk_plugin_abi::split_sql_statements(sql)
}

/// Typed SQL `NULL` for proxy columns so SeaORM `Option<T>` decoding works.
///
/// # Arguments
///
/// * `decl_type` - Declared column type when the engine provides one.
/// * `column` - Column name used when `decl_type` is missing.
#[must_use]
pub fn typed_null(decl_type: Option<&str>, column: &str) -> Value {
    if let Some(decl) = decl_type {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            return Value::BigInt(None);
        }
        if d.contains("BOOL") {
            return Value::Bool(None);
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

/// Typed SQL `NULL` from a well-known column name when `decl_type` is missing.
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
