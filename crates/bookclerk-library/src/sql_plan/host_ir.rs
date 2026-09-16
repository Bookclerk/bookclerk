//! Domain result-selection metadata kept beside a typed [`bookclerk_plugin_abi::ExecuteRequest`].
//!
//! The planner emits [`bookclerk_plugin_abi::ExecuteRequest`] directly. This
//! module holds only the host-side indexes used to interpret
//! [`bookclerk_plugin_abi::ExecuteReply`] into a named [`crate::DbAtomicResult`].

use bookclerk_plugin_abi::{DbColumn, DbRow, DbValue};
use serde_json::Value as JsonValue;

/// Host indexes into a typed atomic batch (not part of the plugin ABI).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AtomicSelection {
    /// Index of the application-status `SELECT` when receipts are not used.
    pub outcome_index: u32,
    /// Index of a payload `SELECT` when the op returns a JSON record.
    pub payload_index: Option<u32>,
    /// Index of the receipt `SELECT` immediately after prune (replay detect).
    pub prior_receipt_index: Option<u32>,
    /// Index of the final receipt `SELECT`.
    pub receipt_select_index: Option<u32>,
}

/// Projects typed result rows onto named JSON objects for domain interpretation.
#[must_use]
pub fn typed_rows_to_json(columns: &[DbColumn], rows: &[DbRow]) -> Vec<JsonValue> {
    rows.iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (col, val) in columns.iter().zip(row.values.iter()) {
                obj.insert(col.name.clone(), db_value_to_domain_json(val));
            }
            JsonValue::Object(obj)
        })
        .collect()
}

/// Encodes one typed cell as domain JSON (nulls stay JSON null; bytes use `b64:`).
fn db_value_to_domain_json(v: &DbValue) -> JsonValue {
    match v {
        DbValue::Null(_) => JsonValue::Null,
        DbValue::Boolean(b) => JsonValue::Bool(*b),
        DbValue::Int64(n) => serde_json::json!(*n),
        DbValue::Float64(n) => serde_json::json!(*n),
        DbValue::Text(s) => JsonValue::String(s.clone()),
        DbValue::Bytes(b) => JsonValue::String(bookclerk_db_exec::bytes_to_b64_string(b)),
    }
}
