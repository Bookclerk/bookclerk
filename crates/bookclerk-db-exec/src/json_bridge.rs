//! Domain JSON projection for typed [`DbValue`] cells.

use bookclerk_plugin_abi::{DbType, DbValue};

/// Converts JSON onto [`DbValue`] for leftover adapter-edge helpers.
///
/// JSON strings are always [`DbValue::Text`]. Decode blob fields with
/// [`db_value_from_b64_json`].
///
/// # Errors
///
/// Returns a static reason when the JSON value is outside the universal
/// domain, or TEXT contains U+0000.
pub fn db_value_from_json(v: &serde_json::Value) -> Result<DbValue, String> {
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
            bookclerk_plugin_abi::require_portable_text(s).map_err(|err| err.to_string())?;
            Ok(DbValue::Text(s.clone()))
        }
        serde_json::Value::Array(_) => Err("arrays are not a baseline DbValue".into()),
        serde_json::Value::Object(_) => Err("objects are not a baseline DbValue".into()),
    }
}

/// Decode a domain JSON string that is known to be a `b64:` blob field.
///
/// Use this only for encoded blob columns (`ciphertext`, `kdf_salt`,
/// `cipher_nonce`, …). Generic JSON strings stay [`DbValue::Text`] via
/// [`db_value_from_json`].
///
/// # Errors
///
/// Returns when the string is not a valid `b64:` payload.
pub fn db_value_from_b64_json(s: &str) -> Result<DbValue, String> {
    crate::b64_string_to_bytes(s)
        .map(DbValue::Bytes)
        .ok_or_else(|| format!("invalid b64: payload: {s}"))
}

/// Encodes [`DbValue`] as domain JSON (typed nulls become JSON null).
#[must_use]
pub fn db_value_to_json(v: &DbValue) -> serde_json::Value {
    match v {
        DbValue::Null(_) => serde_json::Value::Null,
        DbValue::Boolean(b) => serde_json::Value::Bool(*b),
        DbValue::Int64(n) => serde_json::json!(*n),
        DbValue::Float64(n) => serde_json::json!(*n),
        DbValue::Text(s) => serde_json::Value::String(s.clone()),
        DbValue::Bytes(b) => serde_json::Value::String(crate::bytes_to_b64_string(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_and_bytes_roundtrip_without_sea_null() {
        assert_eq!(
            db_value_to_json(&DbValue::Null(DbType::Bytes)),
            serde_json::Value::Null
        );
        assert_eq!(
            db_value_from_json(&json!("b64:YWJj")).unwrap(),
            DbValue::Text("b64:YWJj".into())
        );
        assert_eq!(
            db_value_from_b64_json("b64:AA==").unwrap(),
            DbValue::Bytes(vec![0])
        );
        assert_eq!(
            db_value_to_json(&DbValue::Bytes(vec![0])),
            json!("b64:AA==")
        );
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
