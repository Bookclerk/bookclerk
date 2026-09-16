//! JSON helpers for role-method payloads (storefront / CLI DTOs).
//!
//! These are not an author ABI. Product guests implement [`crate::PluginRoot`]
//! and role traits directly; JSON is only the versioned escape hatch on typed
//! Cap'n Proto methods.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::PluginError;
use bookclerk_plugin_abi::{QueryPage, MAX_LIST_PAGE, MAX_SCALAR_BYTES};

/// Deserialize a role-method JSON argument.
///
/// Empty input is treated as `{}`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the payload is not valid JSON for `T`.
pub fn decode<T: DeserializeOwned>(json: &str) -> Result<T, PluginError> {
    if json.trim().is_empty() {
        return serde_json::from_value(serde_json::Value::Object(Default::default()))
            .map_err(|err| PluginError::invalid_params(err.to_string()));
    }
    serde_json::from_str(json).map_err(|err| PluginError::invalid_params(err.to_string()))
}

/// Serialize a role-method JSON result.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when serialization fails, or
/// [`PluginError::payload_too_large`] when the encoded size exceeds
/// [`MAX_SCALAR_BYTES`].
pub fn encode<T: Serialize>(value: T) -> Result<String, PluginError> {
    let json = serde_json::to_string(&value)
        .map_err(|err| PluginError::invalid_params(err.to_string()))?;
    if json.len() > MAX_SCALAR_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "JSON result {} bytes exceeds {MAX_SCALAR_BYTES}",
            json.len()
        )));
    }
    Ok(json)
}

/// Encodes an atomic result; an oversized scalar after commit is `unavailable`.
///
/// # Errors
///
/// Returns [`PluginError::payload_too_large`] mapped to
/// [`PluginError::unavailable`] so the host retries the same `operationId`.
pub fn encode_atomic_result<T: Serialize>(value: T) -> Result<String, PluginError> {
    match encode(value) {
        Ok(json) => Ok(json),
        Err(err) if err.code == crate::PluginErrorCode::PayloadTooLarge => {
            Err(PluginError::unavailable(err.message.clone()))
        }
        Err(err) => Err(err),
    }
}

/// Page a JSON array of rows. `limit == 0` means [`MAX_LIST_PAGE`].
///
/// # Errors
///
/// Returns [`PluginError::invalid_cursor`] for a non-numeric cursor, or
/// [`PluginError::payload_too_large`] when the page exceeds [`MAX_SCALAR_BYTES`].
pub fn page_rows<T: Serialize>(
    rows: &[T],
    cursor: &str,
    limit: u32,
) -> Result<QueryPage, PluginError> {
    let offset = if cursor.trim().is_empty() {
        0usize
    } else {
        cursor
            .parse::<usize>()
            .map_err(|_| PluginError::invalid_cursor(format!("invalid query cursor `{cursor}`")))?
    };
    let page_size = if limit == 0 {
        MAX_LIST_PAGE as usize
    } else {
        (limit as usize).min(MAX_LIST_PAGE as usize).max(1)
    };
    if offset > rows.len() {
        return Err(PluginError::invalid_cursor(format!(
            "query cursor `{cursor}` is past the result set"
        )));
    }
    let end = (offset + page_size).min(rows.len());
    let page = &rows[offset..end];
    let rows_json = encode(page)?;
    let next_cursor = if end < rows.len() {
        Some(end.to_string())
    } else {
        None
    };
    Ok(QueryPage {
        rows_json,
        next_cursor,
    })
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn page_rows_splits_above_max_list_page() {
        let rows: Vec<u32> = (0..300).collect();
        let first = page_rows(&rows, "", 0).unwrap();
        let first_vals: Vec<u32> = serde_json::from_str(&first.rows_json).unwrap();
        assert_eq!(first_vals.len(), MAX_LIST_PAGE as usize);
        assert_eq!(first_vals[0], 0);
        assert_eq!(first.next_cursor.as_deref(), Some("256"));
        let second = page_rows(&rows, first.next_cursor.as_deref().unwrap(), 0).unwrap();
        let second_vals: Vec<u32> = serde_json::from_str(&second.rows_json).unwrap();
        assert_eq!(second_vals, (256..300).collect::<Vec<u32>>());
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn page_rows_rejects_non_numeric_and_past_end_cursors() {
        let rows = vec![1, 2, 3];
        assert!(page_rows(&rows, "nope", 10).is_err());
        assert!(page_rows(&rows, "4", 10).is_err());
        let empty = page_rows(&rows, "3", 10).unwrap();
        let vals: Vec<u32> = serde_json::from_str(&empty.rows_json).unwrap();
        assert!(vals.is_empty());
        assert!(empty.next_cursor.is_none());
    }

    #[test]
    fn decode_empty_object_and_encode_bound() {
        let v: serde_json::Value = decode("").unwrap();
        assert_eq!(v, serde_json::json!({}));
        let ok = encode(serde_json::json!({"ok": true})).unwrap();
        assert!(ok.contains("ok"));
    }
}
