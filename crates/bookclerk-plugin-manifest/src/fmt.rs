//! Canonical `plugin.toml` formatting for SDK `fmt` / `fmt --check`.
//!
//! Serializes a validated [`crate::PluginManifest`] with `toml::to_string_pretty`
//! and ensures a trailing newline so `fmt --check` diffs stay stable across tools.

use crate::error::Result;
use crate::types::PluginManifest;

/// Serializes a validated manifest to canonical TOML.
///
/// The output is suitable for rewriting `plugin.toml` on disk or comparing
/// against the on-disk file in `fmt --check`. Field order and pretty-print
/// style follow `toml` crate defaults; a trailing `\n` is always present.
///
/// # Arguments
///
/// * `manifest` - Manifest that has already passed [`PluginManifest::validate`]
///   (typically from [`PluginManifest::parse`]).
///
/// # Returns
///
/// Pretty-printed TOML ending with a newline.
///
/// # Errors
///
/// Returns [`crate::Error::TomlSer`] when serialization fails, or a message
/// error when `[queues]` contains a nested table that would otherwise be
/// dropped.
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_manifest::{format_manifest, parse};
///
/// let m = parse(r#"
/// api_version = 3
/// id = "echo"
/// runtime = "native"
/// command = "./echo"
/// entrypoints = ["cli"]
///
/// [capabilities.network]
/// mode = "deny"
/// "#).unwrap();
/// let formatted = format_manifest(&m).unwrap();
/// assert!(formatted.ends_with('\n'));
/// assert_eq!(parse(&formatted).unwrap(), m);
/// ```
pub fn format_manifest(manifest: &PluginManifest) -> Result<String> {
    if let Some(queues) = &manifest.queues {
        ensure_queues_formattable(queues)?;
    }
    let mut out = toml::to_string_pretty(manifest)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Reject `[queues]` values the TypeScript and Python formatters cannot emit.
///
/// A non-empty array of flat records becomes `[[queues.key]]`. Scalars and
/// empty arrays stay inline. A nested table is an error in all three tools.
///
/// # Errors
///
/// Returns a message error when `[queues]` is not a table, or when a field
/// (or a field inside an array-of-tables row) is a nested table.
fn ensure_queues_formattable(value: &toml::Value) -> Result<()> {
    let Some(table) = value.as_table() else {
        return Err(crate::Error::message(
            "plugin.toml: [queues] nested table cannot be formatted",
        ));
    };
    for (key, field) in table {
        if is_queue_array_of_records(field) {
            let Some(rows) = field.as_array() else {
                continue;
            };
            for row in rows {
                let Some(record) = row.as_table() else {
                    continue;
                };
                for (field_name, field_value) in record {
                    if !is_inline_toml_value(field_value) {
                        return Err(crate::Error::message(format!(
                            "plugin.toml: [queues] `{key}.{field_name}` nested table cannot be formatted"
                        )));
                    }
                }
            }
        } else if !is_inline_toml_value(field) {
            return Err(crate::Error::message(format!(
                "plugin.toml: [queues] `{key}` nested table cannot be formatted"
            )));
        }
    }
    Ok(())
}

/// True for a non-empty array whose elements are all TOML tables.
fn is_queue_array_of_records(value: &toml::Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    !items.is_empty() && items.iter().all(|item| item.is_table())
}

/// True for a scalar or an array of scalars. Nested tables are not inline.
fn is_inline_toml_value(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => true,
        toml::Value::Array(items) => items.iter().all(|item| {
            matches!(
                item,
                toml::Value::String(_)
                    | toml::Value::Integer(_)
                    | toml::Value::Float(_)
                    | toml::Value::Boolean(_)
                    | toml::Value::Datetime(_)
            )
        }),
        toml::Value::Table(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::PluginManifest;

    #[test]
    fn fmt_roundtrip_parse() {
        let raw = r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#;
        let m = PluginManifest::parse(raw).unwrap();
        let formatted = format_manifest(&m).unwrap();
        let again = PluginManifest::parse(&formatted).unwrap();
        assert_eq!(m, again);
    }

    #[test]
    fn queues_format_keeps_scalar_arrays_and_rejects_nested_tables() {
        let raw = r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["cli"]

[queues]
names = ["a"]
empty = []

[[queues.producers]]
binding = "MY_QUEUE"
queue = "jobs"

[capabilities.network]
mode = "deny"
"#;
        let manifest = PluginManifest::parse(raw).unwrap();
        let formatted = format_manifest(&manifest).unwrap();
        let kept = u8::from(formatted.contains("[[queues.producers]]"))
            + u8::from(formatted.contains("names = [\"a\"]"))
            + u8::from(formatted.contains("empty = []"));
        assert_eq!(kept, 3);
        let scalar_tables = u8::from(formatted.contains("[[queues.names]]"))
            + u8::from(formatted.contains("[[queues.empty]]"));
        assert_eq!(scalar_tables, 0);
        let nested = raw.replace(
            "[[queues.producers]]",
            "[queues.meta]\nregion = \"us\"\n\n[[queues.producers]]",
        );
        let nested_manifest = PluginManifest::parse(&nested).unwrap();
        let nested_rejected = format_manifest(&nested_manifest)
            .is_err_and(|err| err.to_string().contains("nested table"));
        assert!(nested_rejected, "{}", u8::from(nested_rejected));
    }
}
