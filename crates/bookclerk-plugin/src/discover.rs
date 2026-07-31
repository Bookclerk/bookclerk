//! Scan plugin directories for `plugin.toml` manifests.
//!
//! Discovery is install-time only (`id`, `kind`, `command`, …). User settings
//! come from the matching `[sources.<id>]` / `[integrations.<id>]` table in
//! `config.toml` and are attached when the plugin is spawned.

use std::path::{Path, PathBuf};

use bookclerk_config::Config;

use crate::manifest::PluginManifest;
use crate::{PluginError, Result};

/// A discovered plugin ready to spawn.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub manifest: PluginManifest,
    /// Directory containing `plugin.toml` (cwd + relative `command` base).
    pub root: PathBuf,
    /// Absolute path to the plugin executable.
    pub command: PathBuf,
}

/// Resolve search roots: `BOOKCLERK_PLUGIN_DIRS` then `$FILES_DIR/plugins`.
#[must_use]
pub fn plugin_search_dirs(config: &Config) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(raw) = std::env::var("BOOKCLERK_PLUGIN_DIRS") {
        for part in std::env::split_paths(&raw) {
            if !part.as_os_str().is_empty() {
                dirs.push(part);
            }
        }
    }
    dirs.push(config.paths().files_dir.join("plugins"));
    dirs
}

/// Discover plugins under the configured search directories.
///
/// Accepts either:
/// - `$dir/plugin.toml` (single plugin at root), or
/// - `$dir/<name>/plugin.toml` (one plugin per subdirectory).
///
/// Two manifests that claim the same `(kind, id)` are a hard error — Bookclerk
/// refuses to start with an ambiguous plugin set.
pub fn discover_plugins(config: &Config) -> Result<Vec<DiscoveredPlugin>> {
    let mut out = Vec::new();
    // kind\0id (lowercased) → first manifest path
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for dir in plugin_search_dirs(config) {
        if !dir.is_dir() {
            continue;
        }
        discover_in_dir(&dir, &mut out, &mut seen)?;
    }
    out.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    Ok(out)
}

fn conflict_key(kind: crate::PluginKind, id: &str) -> String {
    format!("{}\0{}", kind.as_str(), id.trim().to_ascii_lowercase())
}

fn discover_in_dir(
    dir: &Path,
    out: &mut Vec<DiscoveredPlugin>,
    seen: &mut std::collections::HashMap<String, PathBuf>,
) -> Result<()> {
    let root_manifest = dir.join("plugin.toml");
    if root_manifest.is_file() {
        push_manifest(&root_manifest, dir, out, seen)?;
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(path = %dir.display(), %err, "cannot read plugin directory");
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        if manifest_path.is_file() {
            push_manifest(&manifest_path, &path, out, seen)?;
        }
    }
    Ok(())
}

fn push_manifest(
    manifest_path: &Path,
    root: &Path,
    out: &mut Vec<DiscoveredPlugin>,
    seen: &mut std::collections::HashMap<String, PathBuf>,
) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)?;
    let manifest = PluginManifest::parse(&text)?;
    if manifest.api_version > crate::PLUGIN_API_VERSION {
        tracing::warn!(
            id = %manifest.id,
            plugin_api = manifest.api_version,
            host_api = crate::PLUGIN_API_VERSION,
            "plugin api_version newer than host; skipping"
        );
        return Ok(());
    }
    let key = conflict_key(manifest.kind, &manifest.id);
    if let Some(first) = seen.get(&key) {
        return Err(PluginError::message(format!(
            "duplicate {} plugin id `{}`: already claimed by {} and also by {}",
            manifest.kind.as_str(),
            manifest.id,
            first.display(),
            manifest_path.display()
        )));
    }
    seen.insert(key, manifest_path.to_path_buf());
    let command = resolve_command(root, &manifest.command)?;
    if !command.is_file() {
        return Err(PluginError::message(format!(
            "plugin `{}`: command not found at {}",
            manifest.id,
            command.display()
        )));
    }
    out.push(DiscoveredPlugin {
        manifest,
        root: root.to_path_buf(),
        command,
    });
    Ok(())
}

fn resolve_command(root: &Path, command: &Path) -> Result<PathBuf> {
    if command.is_absolute() {
        return Ok(command.to_path_buf());
    }
    Ok(root.join(command))
}

/// Opaque knobs from main `config.toml` for this plugin id (by kind).
#[must_use]
pub fn settings_table(config: &Config, plugin: &DiscoveredPlugin) -> toml::Table {
    match plugin.manifest.kind {
        crate::PluginKind::Source => config
            .sources
            .table(&plugin.manifest.id)
            .cloned()
            .unwrap_or_default(),
        crate::PluginKind::Integration => {
            let mut table = config
                .integrations
                .plugin_table(&plugin.manifest.id)
                .cloned()
                .unwrap_or_default();
            inject_abs_api_key_from_env(&plugin.manifest.id, &mut table);
            table
        }
        crate::PluginKind::Output if plugin.manifest.id == "s3" => {
            output_s3_settings_table(&config.output.s3)
        }
        crate::PluginKind::Output | crate::PluginKind::Database => toml::Table::new(),
    }
}

fn output_s3_settings_table(cfg: &bookclerk_config::OutputS3Config) -> toml::Table {
    match toml::Value::try_from(cfg) {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

/// When ABS config lacks `api_key`, inject `BOOKCLERK_ABS_API_KEY` into the
/// handshake table (plugin processes do not inherit Bookclerk env secrets).
fn inject_abs_api_key_from_env(plugin_id: &str, table: &mut toml::Table) {
    match plugin_id.trim().to_ascii_lowercase().as_str() {
        "audiobookshelf" | "abs" => {}
        _ => return,
    }
    let missing = table
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if !missing {
        return;
    }
    if let Ok(v) = std::env::var("BOOKCLERK_ABS_API_KEY") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            table.insert("api_key".into(), toml::Value::String(trimmed.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn discovers_nested_plugin_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let nested = plugins_root.join("echo");
        fs::create_dir_all(&nested).unwrap();
        let bin = nested.join("echo-bin");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
        fs::write(
            nested.join("plugin.toml"),
            r#"
api_version = 1
id = "echo"
kind = "integration"
command = "./echo-bin"
"#,
        )
        .unwrap();

        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let found = discover_plugins(&cfg).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.id, "echo");
        assert_eq!(found[0].command, bin);

        // Settings come from main config, not the manifest.
        let mut cfg2 = cfg;
        cfg2.integrations
            .plugin_table_mut("echo")
            .insert("greeting".into(), toml::Value::String("hi".into()));
        let settings = settings_table(&cfg2, &found[0]);
        assert_eq!(
            settings.get("greeting").and_then(|v| v.as_str()),
            Some("hi")
        );
    }

    fn write_plugin(dir: &Path, id: &str, kind: &str) {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join("bin");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
        fs::write(
            dir.join("plugin.toml"),
            format!(
                r#"
api_version = 1
id = "{id}"
kind = "{kind}"
command = "./bin"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn duplicate_kind_and_id_is_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo-a"), "echo", "integration");
        write_plugin(&plugins.join("echo-b"), "echo", "integration");

        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let err = discover_plugins(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("duplicate integration plugin id `echo`"),
            "{err}"
        );
        assert!(err.contains("plugin.toml"), "{err}");
    }

    #[test]
    fn same_id_different_kind_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo-src"), "echo", "source");
        write_plugin(&plugins.join("echo-int"), "echo", "integration");

        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let found = discover_plugins(&cfg).unwrap();
        assert_eq!(found.len(), 2);
    }
}
