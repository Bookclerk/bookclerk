//! Serialize SeaORM statements and values for database plugin RPC.

use std::collections::BTreeMap;

use base64::Engine;
use sea_orm::{ProxyExecResult, ProxyRow, Statement, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

const SEA_NULL_KEY: &str = "$sea_null";

/// SQL + bind parameters crossing the host↔database-guest boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatementDto {
    pub sql: String,
    #[serde(default)]
    pub values: Vec<JsonValue>,
}

/// One result row from [`super::methods::DB_QUERY`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyRowDto {
    pub values: BTreeMap<String, JsonValue>,
}

/// Result of [`super::methods::DB_QUERY`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResultDto {
    pub rows: Vec<ProxyRowDto>,
}

/// Result of [`super::methods::DB_EXECUTE`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecResultDto {
    pub last_insert_id: u64,
    pub rows_affected: u64,
}

/// Params for [`super::methods::DB_CONNECT`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DbConnectParams {
    pub plugin_data_dir: String,
    /// Active backend id: `sqlite`, `d1`, or `postgres`.
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d1_api_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres_url: Option<String>,
    /// Host-only fallback when no side channel is wired (unconfined / best-effort).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_path: Option<String>,
}

#[must_use]
pub fn statement_to_dto(statement: &Statement) -> StatementDto {
    StatementDto {
        sql: statement.sql.clone(),
        values: match &statement.values {
            Some(values) => values.0.iter().map(sea_value_to_json).collect(),
            None => Vec::new(),
        },
    }
}

#[must_use]
pub fn statement_from_dto(dto: StatementDto, backend: sea_orm::DatabaseBackend) -> Statement {
    if dto.values.is_empty() {
        Statement::from_string(backend, dto.sql)
    } else {
        let values: Vec<Value> = dto
            .values
            .iter()
            .map(|v| json_to_sea_value(v, ""))
            .collect();
        Statement::from_sql_and_values(backend, dto.sql, values)
    }
}

#[must_use]
pub fn proxy_rows_from_dto(rows: Vec<ProxyRowDto>) -> Vec<ProxyRow> {
    rows.into_iter()
        .map(|row| ProxyRow {
            values: row
                .values
                .into_iter()
                .map(|(k, v)| {
                    let key = k.clone();
                    (k, json_to_sea_value(&v, &key))
                })
                .collect(),
        })
        .collect()
}

#[must_use]
pub fn proxy_rows_to_dto(rows: Vec<ProxyRow>) -> Vec<ProxyRowDto> {
    rows.into_iter()
        .map(|row| ProxyRowDto {
            values: row
                .values
                .into_iter()
                .map(|(k, v)| (k, sea_value_to_json(&v)))
                .collect(),
        })
        .collect()
}

#[must_use]
pub fn exec_result_from_dto(dto: ExecResultDto) -> ProxyExecResult {
    ProxyExecResult {
        last_insert_id: dto.last_insert_id,
        rows_affected: dto.rows_affected,
    }
}

#[must_use]
pub fn exec_result_to_dto(result: ProxyExecResult) -> ExecResultDto {
    ExecResultDto {
        last_insert_id: result.last_insert_id,
        rows_affected: result.rows_affected,
    }
}

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

#[must_use]
pub fn json_to_sea_value(v: &JsonValue, column: &str) -> Value {
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
                return Value::Bytes(Some(bytes));
            }
            if is_binary_column(column) {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                    return Value::Bytes(Some(bytes));
                }
            }
            Value::String(Some(s.clone()))
        }
        other => Value::String(Some(other.to_string())),
    }
}

fn sea_null_json(kind: &str) -> JsonValue {
    let mut map = serde_json::Map::new();
    map.insert(SEA_NULL_KEY.into(), JsonValue::String(kind.to_string()));
    JsonValue::Object(map)
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

fn typed_null(column: &str) -> Value {
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

fn is_binary_column(column: &str) -> bool {
    matches!(
        column,
        "ciphertext" | "kdf_salt" | "cipher_nonce" | "vector"
    )
}

#[must_use]
pub fn bytes_to_b64_string(bytes: &[u8]) -> String {
    format!(
        "b64:{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[must_use]
pub fn b64_string_to_bytes(s: &str) -> Option<Vec<u8>> {
    let rest = s.strip_prefix("b64:")?;
    base64::engine::general_purpose::STANDARD
        .decode(rest.as_bytes())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        let raw = b"hello";
        let encoded = bytes_to_b64_string(raw);
        assert_eq!(
            b64_string_to_bytes(&encoded).as_deref(),
            Some(raw.as_slice())
        );
    }

    #[test]
    fn statement_roundtrip_empty_values() {
        let stmt = Statement::from_string(sea_orm::DatabaseBackend::Sqlite, "SELECT 1");
        let dto = statement_to_dto(&stmt);
        let back = statement_from_dto(dto, sea_orm::DatabaseBackend::Sqlite);
        assert_eq!(back.sql, stmt.sql);
    }

    #[test]
    fn typed_null_bytes_roundtrip() {
        let values = [Value::Bytes(None)];
        let dto = StatementDto {
            sql: "INSERT INTO encrypted_secrets (kdf_salt) VALUES (?)".into(),
            values: values.iter().map(sea_value_to_json).collect(),
        };
        let stmt = statement_from_dto(dto, sea_orm::DatabaseBackend::Postgres);
        assert!(matches!(
            stmt.values.as_ref().unwrap().0[0],
            Value::Bytes(None)
        ));
    }
}
