//! Shared oracle for the `instance_config` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is a plugin instance's settings as an operator sends them:
//! the `settings` object of `PUT /api/config/plugin-instances/{id}/config`,
//! decoded the way the route decodes it and checked by
//! [`PluginInstanceConfigV1::validate`], and the text of
//! `bookclerk plugins config set`, read by [`parse_setting_text`]. Both now
//! carry table settings, rows of records.
//!
//! The input's first byte picks the path (even: the JSON object, odd: the
//! CLI text for the table setting) and the rest is the payload. The fixed
//! schema declares a string setting `name` and a table `mappings` with
//! columns `library` (string, required), `prefix` (string), `watch` (bool,
//! default `true`) and `mode` (enum `read` / `write`).
//!
//! The invariant, checked against this file's own reading of the payload
//! (an untyped JSON value, or its own split of the row text):
//!
//! - The JSON object is accepted exactly when it has at most 32 settings,
//!   each a declared key, `name` a string of at most 1024 bytes with no NUL,
//!   and `mappings` an array of at most 64 objects whose fields are declared
//!   columns of the column's kind (strings at most 1024 bytes, no NUL), each
//!   row naming `library`. An accepted `mappings` normalizes to itself with
//!   `watch` filled, and normalizing again changes nothing.
//! - Row text not starting with `[` is split on unescaped `|` (`\|` and
//!   `\\` escape), cells trimmed. It is accepted exactly when it has at
//!   most 4 cells, a non-empty first cell, a `watch` cell (if any) that is
//!   one of the boolean words and a `mode` cell (if any) that is `read` or
//!   `write`, and string cells of at most 1024 bytes with no NUL. The
//!   result is one row holding exactly the non-empty cells.

use std::collections::BTreeMap;

use bookclerk_library::control_plane::{
    parse_setting_text, PluginInstanceConfigV1, SettingCell, SettingValue,
};
use bookclerk_plugin_abi::SettingsSchema;
use serde_json::{json, Value};

/// Largest input worth checking; it keeps an accepted document under the
/// 16 KiB document limit, which the oracle does not restate.
pub const MAX_INPUT_BYTES: usize = 8192;

/// Longest string setting or cell, written out rather than read from the
/// crate.
const MAX_TEXT_BYTES: usize = 1024;

/// The schema every input is checked against.
#[allow(clippy::expect_used)] // A fixed, valid literal.
fn schema() -> SettingsSchema {
    serde_json::from_value(json!({
        "settings": [
            { "key": "name", "type": "string" },
            {
                "key": "mappings",
                "type": "table",
                "columns": [
                    { "key": "library", "type": "string", "required": true },
                    { "key": "prefix", "type": "string" },
                    { "key": "watch", "type": "bool", "default": true },
                    { "key": "mode", "type": "enum", "values": [{ "id": "read" }, { "id": "write" }] }
                ]
            }
        ]
    }))
    .expect("the oracle's schema parses")
}

/// A string setting or cell the schema accepts.
fn text_ok(text: &str) -> bool {
    text.len() <= MAX_TEXT_BYTES && !text.contains('\0')
}

/// Whether one row (as JSON) fits the table's columns.
fn row_ok(row: &Value) -> bool {
    let Some(fields) = row.as_object() else {
        return false;
    };
    let fields_ok = fields.iter().all(|(column, cell)| match column.as_str() {
        "library" | "prefix" => cell.as_str().is_some_and(text_ok),
        "watch" => cell.is_boolean(),
        "mode" => matches!(cell.as_str(), Some("read" | "write")),
        _ => false,
    });
    fields_ok && fields.contains_key("library")
}

/// Whether every value in a row is a scalar, so it decodes as a row at all.
fn row_is_scalars(row: &Value) -> bool {
    row.as_object().is_some_and(|fields| {
        fields
            .values()
            .all(|cell| cell.is_boolean() || cell.is_number() || cell.is_string())
    })
}

/// The oracle's verdict on a settings object.
fn settings_expected(settings: &serde_json::Map<String, Value>) -> bool {
    if settings.len() > 32 {
        return false;
    }
    settings.iter().all(|(key, value)| match key.as_str() {
        "name" => value.as_str().is_some_and(text_ok),
        "mappings" => value.as_array().is_some_and(|rows| {
            rows.len() <= 64 && rows.iter().all(row_is_scalars) && rows.iter().all(row_ok)
        }),
        _ => false,
    })
}

/// Checks the JSON path on one payload.
#[allow(clippy::expect_used)] // `mappings` is declared and normalizes to rows.
fn check_document(payload: &[u8]) -> bool {
    let schema = schema();
    let untyped: Option<serde_json::Map<String, Value>> = serde_json::from_slice(payload).ok();
    let decoded: Option<BTreeMap<String, SettingValue>> = serde_json::from_slice(payload).ok();
    let accepted = decoded.as_ref().is_some_and(|settings| {
        PluginInstanceConfigV1 {
            settings: settings.clone(),
            secret_refs: Vec::new(),
        }
        .validate(&schema)
        .is_ok()
    });
    let expected = untyped.as_ref().is_some_and(settings_expected);
    assert_eq!(accepted, expected, "settings verdict for {untyped:?}");
    if accepted {
        let Some(settings) = decoded else {
            return accepted;
        };
        if let Some(mappings) = settings.get("mappings") {
            let spec = schema.get("mappings").expect("declared");
            let normalized = spec.normalize_value(&mappings.to_json());
            assert!(spec.check_value(&normalized).is_ok());
            assert_eq!(spec.normalize_value(&normalized), normalized, "idempotent");
            let rows = normalized.as_array().expect("rows");
            assert!(
                rows.iter().all(|row| row["watch"].is_boolean()),
                "watch filled"
            );
        }
    }
    accepted
}

/// The oracle's own split of row text.
#[allow(clippy::expect_used)] // `cells` starts with one entry and only grows.
fn split(text: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut escaped = false;
    for ch in text.chars() {
        let current = cells.last_mut().expect("never empty");
        if escaped {
            if ch != '|' && ch != '\\' {
                current.push('\\');
            }
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(String::new());
        } else {
            current.push(ch);
        }
    }
    if escaped {
        cells.last_mut().expect("never empty").push('\\');
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// The row the oracle expects from row text, or `None` for a refusal.
fn row_expected(text: &str) -> Option<BTreeMap<String, SettingCell>> {
    let cells = split(text);
    if cells.len() > 4 {
        return None;
    }
    let mut row = BTreeMap::new();
    for (column, cell) in ["library", "prefix", "watch", "mode"].iter().zip(cells) {
        if cell.is_empty() {
            continue;
        }
        let value = match *column {
            "watch" => match cell.as_str() {
                "true" | "1" | "yes" | "on" => SettingCell::Bool(true),
                "false" | "0" | "no" | "off" => SettingCell::Bool(false),
                _ => return None,
            },
            "mode" if cell == "read" || cell == "write" => SettingCell::String(cell),
            "mode" => return None,
            _ if text_ok(&cell) => SettingCell::String(cell),
            _ => return None,
        };
        row.insert((*column).to_string(), value);
    }
    row.contains_key("library").then_some(row)
}

/// Checks the CLI text path on one payload.
fn check_text(payload: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(payload) else {
        return false;
    };
    let parsed = parse_setting_text(&schema(), "mappings", text);
    if text.trim_start().starts_with('[') {
        // The JSON form: accepted only as rows the schema accepts.
        if let Ok(value) = &parsed {
            assert!(matches!(value, SettingValue::Rows(_)), "{value:?}");
            let json = value.to_json();
            assert!(json.as_array().is_some_and(|rows| rows.iter().all(row_ok)));
        }
        return parsed.is_ok();
    }
    let expected = row_expected(text);
    match (&parsed, &expected) {
        (Ok(SettingValue::Rows(rows)), Some(row)) => {
            assert_eq!(
                rows.as_slice(),
                std::slice::from_ref(row),
                "row for {text:?}"
            )
        }
        (Err(_), None) => {}
        _ => panic!("row text {text:?}: parsed {parsed:?}, expected {expected:?}"),
    }
    parsed.is_ok()
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Path selector byte, then the payload.
///
/// # Returns
///
/// Whether the input was accepted, so the replay test can tell that the
/// seeds reach both sides.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Some((&selector, payload)) = data.split_first() else {
        return false;
    };
    if selector % 2 == 0 {
        check_document(payload)
    } else {
        check_text(payload)
    }
}
