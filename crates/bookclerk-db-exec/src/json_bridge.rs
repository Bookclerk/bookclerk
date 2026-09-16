//! Legacy JSON bind bridge for host SeaORM proxy paths (adapter edge only).

use bookclerk_plugin_abi::{DbType, DbValue};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use crate::host_ir::{sea_null, sea_null_kind};

/// Converts a legacy JSON bind onto [`DbValue`].
///
/// # Errors
///
/// Returns a static reason when the JSON value is outside the universal
/// domain, or TEXT contains U+0000.
pub fn db_value_from_json(v: &serde_json::Value) -> Result<DbValue, String> {
    if let Some(kind) = sea_null_kind(v) {
        let ty = match kind {
            "Bytes" => DbType::Bytes,
            "BigInt" | "Int" | "TinyInt" | "SmallInt" | "TinyUnsigned" | "SmallUnsigned"
            | "Unsigned" | "BigUnsigned" => DbType::Int64,
            "Bool" => DbType::Bool,
            "Double" | "Float" => DbType::Float64,
            _ => DbType::Text,
        };
        return Ok(DbValue::Null(ty));
    }
    match v {
        serde_json::Value::Null => Ok(DbValue::Null(DbType::Unspecified)),
        serde_json::Value::Bool(b) => Ok(DbValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(DbValue::Int64(i));
            }
            if let Some(u) = n.as_u64() {
                let i = i64::try_from(u)
                    .map_err(|_| format!("unsigned integer {u} overflows int64"))?;
                return Ok(DbValue::Int64(i));
            }
            let f = n
                .as_f64()
                .ok_or_else(|| "number is not a finite float64".to_string())?;
            if !f.is_finite() {
                return Err("float64 value is not finite".into());
            }
            Ok(DbValue::Float64(f))
        }
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("b64:") {
                let bytes = BASE64
                    .decode(rest)
                    .map_err(|err| format!("invalid b64: payload: {err}"))?;
                return Ok(DbValue::Bytes(bytes));
            }
            bookclerk_plugin_abi::require_portable_text(s).map_err(|err| err.to_string())?;
            Ok(DbValue::Text(s.clone()))
        }
        serde_json::Value::Array(_) => Err("arrays are not a baseline DbValue".into()),
        serde_json::Value::Object(_) => Err("objects are not a baseline DbValue".into()),
    }
}

/// Encodes [`DbValue`] as the legacy JSON bind used by in-process executors.
#[must_use]
pub fn db_value_to_json(v: &DbValue) -> serde_json::Value {
    match v {
        DbValue::Null(DbType::Bytes) => sea_null("Bytes"),
        DbValue::Null(DbType::Int64) => sea_null("BigInt"),
        DbValue::Null(DbType::Bool) => sea_null("Bool"),
        DbValue::Null(DbType::Float64) => sea_null("Double"),
        DbValue::Null(DbType::Text | DbType::Unspecified) => serde_json::Value::Null,
        DbValue::Boolean(b) => serde_json::Value::Bool(*b),
        DbValue::Int64(n) => serde_json::json!(*n),
        DbValue::Float64(n) => serde_json::json!(*n),
        DbValue::Text(s) => serde_json::Value::String(s.clone()),
        DbValue::Bytes(b) => serde_json::Value::String(format!("b64:{}", BASE64.encode(b))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_null_bytes_roundtrip() {
        let v = db_value_from_json(&sea_null("Bytes")).unwrap();
        assert_eq!(v, DbValue::Null(DbType::Bytes));
        assert_eq!(sea_null_kind(&db_value_to_json(&v)), Some("Bytes"));
    }

    #[test]
    fn arrays_are_rejected() {
        let err = db_value_from_json(&json!([1, 2])).unwrap_err();
        assert!(err.contains("arrays"), "{err}");
    }

    #[test]
    fn text_nul_is_rejected() {
        let err = db_value_from_json(&json!("a\u{0000}b")).unwrap_err();
        assert!(err.contains("U+0000"), "{err}");
    }
}
