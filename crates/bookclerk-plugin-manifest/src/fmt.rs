//! Canonical `plugin.toml` formatting.

use crate::error::Result;
use crate::types::PluginManifest;

/// Serialize a validated manifest to canonical TOML (stable for `fmt --check`).
pub fn format_manifest(manifest: &PluginManifest) -> Result<String> {
    let mut out = toml::to_string_pretty(manifest)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginManifest;

    #[test]
    fn fmt_roundtrip_parse() {
        let raw = r#"
api_version = 1
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
