//! SeaORM ↔ typed [`bookclerk_plugin_abi::DbValue`] bridge for database guests.
//!
//! **Feature gate:** compile with `bookclerk-plugin-sdk` feature `db` (pulls
//! `sea-orm` + `base64`). Use these helpers when shuttling SQL through the
//! Cap'n Proto typed execute contract.

use sea_orm::{ProxyRow, Value};

/// Fallible SeaORM → [`bookclerk_plugin_abi::DbValue`] bridge.
///
/// # Errors
///
/// Returns a static reason when the SeaORM value is outside the universal
/// domain, or TEXT contains U+0000.
#[allow(dead_code)] // public cross-crate bridge (database plugin integration tests)
pub fn db_value_from_sea(v: &Value) -> Result<bookclerk_plugin_abi::DbValue, String> {
    use bookclerk_plugin_abi::{DbType, DbValue};
    match v {
        Value::Bool(Some(b)) => Ok(DbValue::Boolean(*b)),
        Value::TinyInt(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        Value::SmallInt(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        Value::Int(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        Value::BigInt(Some(n)) => Ok(DbValue::Int64(*n)),
        Value::TinyUnsigned(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        Value::SmallUnsigned(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        Value::Unsigned(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        Value::BigUnsigned(Some(n)) => i64::try_from(*n)
            .map(DbValue::Int64)
            .map_err(|_| format!("unsigned integer {n} overflows int64")),
        Value::Float(Some(n)) => {
            let f = f64::from(*n);
            if !f.is_finite() {
                return Err("float64 value is not finite".into());
            }
            Ok(DbValue::Float64(f))
        }
        Value::Double(Some(n)) => {
            if !n.is_finite() {
                return Err("float64 value is not finite".into());
            }
            Ok(DbValue::Float64(*n))
        }
        Value::String(Some(s)) => {
            bookclerk_plugin_abi::require_portable_text(s).map_err(|err| err.to_string())?;
            Ok(DbValue::Text(s.to_string()))
        }
        Value::Char(Some(c)) => {
            let s = c.to_string();
            bookclerk_plugin_abi::require_portable_text(&s).map_err(|err| err.to_string())?;
            Ok(DbValue::Text(s))
        }
        Value::Bytes(Some(b)) => Ok(DbValue::Bytes(b.to_vec())),
        Value::ChronoDateTimeUtc(Some(dt)) => Ok(DbValue::Text(dt.to_rfc3339())),
        Value::ChronoDateTime(Some(dt)) => Ok(DbValue::Text(dt.and_utc().to_rfc3339())),
        Value::ChronoDate(Some(d)) => Ok(DbValue::Text(d.to_string())),
        Value::ChronoTime(Some(t)) => Ok(DbValue::Text(t.to_string())),
        Value::ChronoDateTimeWithTimeZone(Some(dt)) => Ok(DbValue::Text(dt.to_rfc3339())),
        Value::ChronoDateTimeLocal(Some(dt)) => Ok(DbValue::Text(dt.to_rfc3339())),
        Value::Uuid(Some(u)) => Ok(DbValue::Text(u.to_string())),
        Value::Json(Some(_)) => Err("json is not a baseline DbValue".into()),
        Value::Enum(_) => Err("enums are not a baseline DbValue".into()),
        Value::Array(_, _) => Err("arrays are not a baseline DbValue".into()),
        Value::Bool(None) => Ok(DbValue::Null(DbType::Bool)),
        Value::TinyInt(None)
        | Value::SmallInt(None)
        | Value::Int(None)
        | Value::BigInt(None)
        | Value::TinyUnsigned(None)
        | Value::SmallUnsigned(None)
        | Value::Unsigned(None)
        | Value::BigUnsigned(None) => Ok(DbValue::Null(DbType::Int64)),
        Value::Float(None) | Value::Double(None) => Ok(DbValue::Null(DbType::Float64)),
        Value::Bytes(None) => Ok(DbValue::Null(DbType::Bytes)),
        Value::String(None)
        | Value::Char(None)
        | Value::ChronoDateTimeUtc(None)
        | Value::ChronoDateTime(None)
        | Value::ChronoDate(None)
        | Value::ChronoTime(None)
        | Value::ChronoDateTimeWithTimeZone(None)
        | Value::ChronoDateTimeLocal(None)
        | Value::Json(None)
        | Value::Uuid(None) => Ok(DbValue::Null(DbType::Text)),
    }
}

/// Convert a typed bind into a SeaORM value.
#[must_use]
pub fn db_value_to_sea(value: &bookclerk_plugin_abi::DbValue) -> Value {
    use bookclerk_plugin_abi::{DbType, DbValue};
    match value {
        DbValue::Null(DbType::Unspecified | DbType::Text) => Value::String(None),
        DbValue::Null(DbType::Int64) => Value::BigInt(None),
        DbValue::Null(DbType::Float64) => Value::Double(None),
        DbValue::Null(DbType::Bytes) => Value::Bytes(None),
        DbValue::Null(DbType::Bool) => Value::Bool(None),
        DbValue::Text(s) => Value::String(Some(s.clone())),
        DbValue::Int64(n) => Value::BigInt(Some(*n)),
        DbValue::Float64(n) => Value::Double(Some(*n)),
        DbValue::Bytes(b) => Value::Bytes(Some(b.clone())),
        DbValue::Boolean(b) => Value::Bool(Some(*b)),
    }
}

/// Builds SeaORM proxy rows from a typed statement result.
///
/// # Errors
///
/// Returns when the result is not positional.
pub fn proxy_rows_from_typed(
    stmt: &bookclerk_plugin_abi::StatementResult,
) -> Result<Vec<ProxyRow>, String> {
    stmt.validate_positional()?;
    Ok(stmt
        .rows
        .iter()
        .map(|row| ProxyRow {
            values: stmt
                .columns
                .iter()
                .zip(row.values.iter())
                .map(|(col, v)| (col.name.clone(), db_value_to_sea(v)))
                .collect(),
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn sea_bridge_coalesces_ints_and_rejects_arrays() {
        assert_eq!(
            db_value_from_sea(&Value::TinyInt(Some(-3))).unwrap(),
            bookclerk_plugin_abi::DbValue::Int64(-3)
        );
        assert!(db_value_from_sea(&Value::Array(
            sea_orm::sea_query::ArrayType::Int,
            Some(Box::new(vec![Value::Int(Some(1))]))
        ))
        .unwrap_err()
        .contains("arrays"));
    }
}
