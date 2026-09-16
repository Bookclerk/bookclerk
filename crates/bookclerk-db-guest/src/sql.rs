//! In-process SeaORM session SQL types (not public ABI wire DTOs).

#![allow(clippy::missing_docs_in_private_items, dead_code)]

use std::collections::BTreeMap;

use base64::Engine;
use sea_orm::{from_query_result_to_proxy_row, DatabaseBackend, Statement, Value};
use serde_json::Value as JsonValue;

/// JSON object key for a typed SQL null (`{"$sea_null": "Bytes"}`).
///
/// This encoding is private to the in-process SeaORM session worker. It is
/// not the host IR / plugin ABI wire format.
const SEA_NULL_KEY: &str = "$sea_null";

/// Wire JSON for a typed null bind of SeaORM `kind`.
fn sea_null(kind: &str) -> JsonValue {
    let mut map = serde_json::Map::new();
    map.insert(SEA_NULL_KEY.into(), JsonValue::String(kind.to_string()));
    JsonValue::Object(map)
}

/// SQL text, JSON-encoded binds, and optional txn id for the session worker.
#[derive(Debug, Clone)]
pub struct GuestStatement {
    /// SQL text with positional placeholders.
    pub sql: String,
    /// Ordered bind values (JSON encoding for typed nulls and bytes).
    pub values: Vec<JsonValue>,
    /// Active guest transaction id, if any.
    pub txn_id: Option<String>,
}

/// One query row projected for session paging and tests.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GuestRow {
    /// Column name → JSON cell value.
    pub values: BTreeMap<String, JsonValue>,
}

/// Read-only statement result inside the session worker.
#[derive(Debug, Clone)]
pub struct GuestQueryResult {
    /// Result rows in engine order.
    pub rows: Vec<GuestRow>,
}

/// Mutating statement outcome inside the session worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestExecResult {
    /// Last auto-increment value when applicable.
    pub last_insert_id: u64,
    /// Rows affected by the statement.
    pub rows_affected: u64,
}

/// Builds a [`GuestStatement`] for plain SQL without binds.
#[must_use]
pub fn guest_sql(sql: impl Into<String>) -> GuestStatement {
    GuestStatement {
        sql: sql.into(),
        values: Vec::new(),
        txn_id: None,
    }
}

/// Rebuilds a SeaORM [`Statement`] after adapter-side canonical SQL lowering.
#[must_use]
pub fn guest_statement_to_seaorm(stmt: GuestStatement, backend: DatabaseBackend) -> Statement {
    let sql = bookclerk_db_exec::lower_canonical_sql(backend, &stmt.sql);
    if stmt.values.is_empty() {
        Statement::from_string(backend, sql)
    } else {
        let values: Vec<Value> = stmt
            .values
            .iter()
            .map(|v| json_to_sea_value(v, ""))
            .collect();
        Statement::from_sql_and_values(backend, sql, values)
    }
}

/// Projects one SeaORM query row into a [`GuestRow`].
#[must_use]
pub fn row_to_guest_row(row: &sea_orm::QueryResult) -> GuestRow {
    let proxy = from_query_result_to_proxy_row(row);
    GuestRow {
        values: proxy
            .values
            .into_iter()
            .map(|(k, v)| (k, sea_value_to_json(&v)))
            .collect(),
    }
}

/// Encodes one SeaORM [`Value`] as JSON for in-process session binds and rows.
#[must_use]
pub fn sea_value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Bool(Some(b)) => JsonValue::Bool(*b),
        Value::TinyInt(Some(n)) => JsonValue::from(*n),
        Value::SmallInt(Some(n)) => JsonValue::from(*n),
        Value::Int(Some(n)) => JsonValue::from(*n),
        Value::BigInt(Some(n)) => JsonValue::from(*n),
        Value::TinyUnsigned(Some(n)) => JsonValue::from(*n),
        Value::SmallUnsigned(Some(n)) => JsonValue::from(*n),
        Value::Unsigned(Some(n)) => JsonValue::from(*n),
        Value::BigUnsigned(Some(n)) => JsonValue::from(*n),
        Value::Float(Some(n)) => JsonValue::from(f64::from(*n)),
        Value::Double(Some(n)) => JsonValue::from(*n),
        Value::String(Some(s)) => JsonValue::String(s.to_string()),
        Value::Char(Some(c)) => JsonValue::String(c.to_string()),
        Value::Bytes(Some(b)) => JsonValue::String(bytes_to_b64_string(b)),
        Value::ChronoDateTimeUtc(Some(dt)) => JsonValue::String(dt.to_rfc3339()),
        Value::ChronoDateTime(Some(dt)) => JsonValue::String(dt.and_utc().to_rfc3339()),
        Value::ChronoDate(Some(d)) => JsonValue::String(d.to_string()),
        Value::ChronoTime(Some(t)) => JsonValue::String(t.to_string()),
        Value::ChronoDateTimeWithTimeZone(Some(dt)) => JsonValue::String(dt.to_rfc3339()),
        Value::Json(Some(j)) => j.as_ref().clone(),
        Value::Uuid(Some(u)) => JsonValue::String(u.to_string()),
        Value::ChronoDateTimeLocal(Some(dt)) => JsonValue::String(dt.to_rfc3339()),
        Value::ChronoDateTimeLocal(None) => sea_null_json("ChronoDateTimeLocal"),
        Value::Enum(e) => match e {
            sea_orm::sea_query::OptionEnum::Some(val) => JsonValue::String(format!("{val:?}")),
            sea_orm::sea_query::OptionEnum::None(_) => sea_null_json("Enum"),
        },
        Value::Array(_, Some(items)) => {
            JsonValue::Array(items.iter().map(sea_value_to_json).collect())
        }
        Value::Array(_, None) => sea_null_json("Array"),
        Value::Bool(None) => sea_null_json("Bool"),
        Value::TinyInt(None) => sea_null_json("TinyInt"),
        Value::SmallInt(None) => sea_null_json("SmallInt"),
        Value::Int(None) => sea_null_json("Int"),
        Value::BigInt(None) => sea_null_json("BigInt"),
        Value::TinyUnsigned(None) => sea_null_json("TinyUnsigned"),
        Value::SmallUnsigned(None) => sea_null_json("SmallUnsigned"),
        Value::Unsigned(None) => sea_null_json("Unsigned"),
        Value::BigUnsigned(None) => sea_null_json("BigUnsigned"),
        Value::Float(None) => sea_null_json("Float"),
        Value::Double(None) => sea_null_json("Double"),
        Value::String(None) => sea_null_json("String"),
        Value::Char(None) => sea_null_json("Char"),
        Value::Bytes(None) => sea_null_json("Bytes"),
        Value::ChronoDateTimeUtc(None) => sea_null_json("ChronoDateTimeUtc"),
        Value::ChronoDateTime(None) => sea_null_json("ChronoDateTime"),
        Value::ChronoDate(None) => sea_null_json("ChronoDate"),
        Value::ChronoTime(None) => sea_null_json("ChronoTime"),
        Value::ChronoDateTimeWithTimeZone(None) => sea_null_json("ChronoDateTimeWithTimeZone"),
        Value::Json(None) => sea_null_json("Json"),
        Value::Uuid(None) => sea_null_json("Uuid"),
    }
}

fn sea_null_json(kind: &str) -> JsonValue {
    sea_null(kind)
}

fn json_sea_null(v: &JsonValue) -> Option<Value> {
    let kind = v.get(SEA_NULL_KEY)?.as_str()?;
    Some(match kind {
        "Bool" => Value::Bool(None),
        "TinyInt" => Value::TinyInt(None),
        "SmallInt" => Value::SmallInt(None),
        "Int" => Value::Int(None),
        "BigInt" => Value::BigInt(None),
        "TinyUnsigned" => Value::TinyUnsigned(None),
        "SmallUnsigned" => Value::SmallUnsigned(None),
        "Unsigned" => Value::Unsigned(None),
        "BigUnsigned" => Value::BigUnsigned(None),
        "Float" => Value::Float(None),
        "Double" => Value::Double(None),
        "String" => Value::String(None),
        "Char" => Value::Char(None),
        "Bytes" => Value::Bytes(None),
        "ChronoDateTimeUtc" => Value::ChronoDateTimeUtc(None),
        "ChronoDateTime" => Value::ChronoDateTime(None),
        "ChronoDate" => Value::ChronoDate(None),
        "ChronoTime" => Value::ChronoTime(None),
        "ChronoDateTimeWithTimeZone" => Value::ChronoDateTimeWithTimeZone(None),
        "Json" => Value::Json(None),
        "Uuid" => Value::Uuid(None),
        "ChronoDateTimeLocal" => Value::ChronoDateTimeLocal(None),
        "Enum" => Value::Enum(sea_orm::sea_query::OptionEnum::None("".into())),
        "Array" => Value::Array(sea_orm::sea_query::ArrayType::String, None),
        _ => Value::String(None),
    })
}

/// Decodes JSON bind values into SeaORM [`Value`] cells.
fn json_to_sea_value(v: &JsonValue, column: &str) -> Value {
    if let Some(value) = json_sea_null(v) {
        return value;
    }
    match v {
        JsonValue::Null => typed_null(column),
        JsonValue::Bool(b) => Value::Bool(Some(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(Some(i))
            } else if let Some(f) = n.as_f64() {
                Value::Double(Some(f))
            } else {
                Value::String(Some(n.to_string()))
            }
        }
        JsonValue::String(s) => {
            if let Some(bytes) = b64_string_to_bytes(s) {
                Value::Bytes(Some(bytes))
            } else if is_binary_column(column) {
                Value::Bytes(Some(s.as_bytes().to_vec()))
            } else {
                Value::String(Some(s.clone()))
            }
        }
        JsonValue::Array(items) => Value::Array(
            sea_orm::sea_query::ArrayType::String,
            Some(Box::new(
                items.iter().map(|v| json_to_sea_value(v, column)).collect(),
            )),
        ),
        JsonValue::Object(_) => Value::String(Some(v.to_string())),
    }
}

fn is_binary_column(column: &str) -> bool {
    matches!(
        column,
        "ciphertext" | "kdf_salt" | "cipher_nonce" | "vector"
    )
}

/// Typed SQL `NULL` for known Bookclerk column names.
#[must_use]
pub fn typed_null(column: &str) -> Value {
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

/// Encodes raw bytes as a `b64:` prefixed string for JSON wire cells.
#[must_use]
pub fn bytes_to_b64_string(bytes: &[u8]) -> String {
    format!(
        "b64:{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Decodes a `b64:` prefixed string back to bytes when the prefix matches.
#[must_use]
pub fn b64_string_to_bytes(s: &str) -> Option<Vec<u8>> {
    let rest = s.strip_prefix("b64:")?;
    base64::engine::general_purpose::STANDARD
        .decode(rest.as_bytes())
        .ok()
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn typed_null_bytes_roundtrip() {
        let values = [Value::Bytes(None)];
        let stmt = guest_statement_to_seaorm(
            GuestStatement {
                sql: "INSERT INTO encrypted_secrets (kdf_salt) VALUES (?)".into(),
                values: values.iter().map(sea_value_to_json).collect(),
                txn_id: None,
            },
            DatabaseBackend::Postgres,
        );
        assert!(matches!(
            stmt.values.as_ref().unwrap().0[0],
            Value::Bytes(None)
        ));
    }
}
