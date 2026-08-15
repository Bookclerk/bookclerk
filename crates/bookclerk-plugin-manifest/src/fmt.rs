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
/// Returns [`crate::Error::TomlSer`] when serialization fails.
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_manifest::{format_manifest, parse};
///
/// let m = parse(r#"
/// api_version = 2
/// id = "echo"
/// kind = "integration"
/// runtime = "native"
/// command = "./echo"
///
/// [capabilities.network]
/// mode = "deny"
/// "#).unwrap();
/// let formatted = format_manifest(&m).unwrap();
/// assert!(formatted.ends_with('\n'));
/// assert_eq!(parse(&formatted).unwrap(), m);
/// ```
pub fn format_manifest(manifest: &PluginManifest) -> Result<String> {
    let mut out = toml::to_string_pretty(manifest)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::PluginManifest;

    #[test]
    fn fmt_roundtrip_parse() {
        let raw = r#"
api_version = 2
id = "echo"
kind = "integration"
runtime = "native"
command = "./echo"

[capabilities.network]
mode = "deny"
"#;
        let m = PluginManifest::parse(raw).unwrap();
        let formatted = format_manifest(&m).unwrap();
        let again = PluginManifest::parse(&formatted).unwrap();
        assert_eq!(m, again);
    }
}
