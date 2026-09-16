//! Universal Cap'n database value domain (`DbValue`).
//!
//! Baseline cells are typed null, bool, int64, finite float64, UTF-8 text, and
//! bytes. SeaORM and JSON bridges live in `bookclerk-db-exec` at the adapter edge.

use serde::{Deserialize, Serialize};

/// Expected SQL type for a typed null (Cap'n `DbType`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum DbType {
    /// Adapter may infer a type (unspecified null).
    #[default]
    Unspecified,
    /// Boolean null.
    Bool,
    /// Signed 64-bit integer null.
    Int64,
    /// Finite 64-bit float null.
    Float64,
    /// UTF-8 text null.
    Text,
    /// Bytea / blob null.
    Bytes,
}

/// One parameter or cell in the universal database domain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum DbValue {
    /// Typed SQL null.
    Null(DbType),
    /// Boolean.
    Boolean(bool),
    /// Signed 64-bit integer.
    Int64(i64),
    /// Finite 64-bit float. Non-finite values are rejected at the bridge.
    Float64(f64),
    /// UTF-8 text.
    Text(String),
    /// Opaque bytes (not a `b64:` string).
    Bytes(Vec<u8>),
}

impl DbValue {
    /// Typed null of `ty`.
    #[must_use]
    pub const fn null(ty: DbType) -> Self {
        Self::Null(ty)
    }
}

/// Maps a declared SQL column type onto the universal [`DbType`].
///
/// Shared by every adapter so declared-type result normalization is
/// identical across engines (SQLite decltype, D1 `pragma_table_info` type,
/// PostgreSQL type names). Unknown declarations map to
/// [`DbType::Unspecified`].
#[must_use]
pub fn db_type_from_declared(decl: &str) -> DbType {
    let d = decl.to_ascii_uppercase();
    if d.contains("BLOB") || d.contains("BYTEA") {
        DbType::Bytes
    } else if d.contains("INT") {
        DbType::Int64
    } else if d.contains("BOOL") {
        DbType::Bool
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        DbType::Float64
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        DbType::Text
    } else {
        DbType::Unspecified
    }
}

/// Normalizes one result cell against its declared column [`DbType`].
///
/// The universal-value contract requires identical observable `DbValue`
/// variants across adapters, so declared-type metadata (never engine storage
/// affinity) decides the variant:
///
/// - every SQL NULL in a declared column becomes `Null(<declared>)`;
/// - `0` / `1` integers in a declared `BOOL` column become [`DbValue::Boolean`]
///   (SQLite-family engines store booleans as INTEGER);
/// - integers in a declared `FLOAT64` column become [`DbValue::Float64`]
///   (JSON channels drop the `.0` on whole floats).
///
/// Cells in undeclared / computed columns are returned unchanged.
#[must_use]
pub fn normalize_db_value_for_column(value: DbValue, declared: DbType) -> DbValue {
    match (declared, value) {
        (DbType::Unspecified, value) => value,
        (ty, DbValue::Null(_)) => DbValue::Null(ty),
        (DbType::Bool, DbValue::Int64(n @ (0 | 1))) => DbValue::Boolean(n == 1),
        (DbType::Float64, DbValue::Int64(n)) => {
            // Declared-double columns already went through the engine's REAL
            // affinity; JSON channels may still render whole floats as ints.
            #[allow(clippy::cast_precision_loss)]
            DbValue::Float64(n as f64)
        }
        (_, value) => value,
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_is_rejected() {
        let err = serde_json::from_str::<DbValue>(r#"{"kind":"xml","value":"<a/>"}"#).unwrap_err();
        assert!(err.to_string().contains("xml") || err.to_string().contains("unknown"));
    }

    #[test]
    fn null_default_roundtrip() {
        let v = DbValue::null(DbType::Text);
        assert_eq!(v, DbValue::Null(DbType::Text));
    }
}
