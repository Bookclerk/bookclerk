//! Scan plugin directories for `plugin.toml` manifests.
//!
//! Discovery is install-time only. User settings come from the matching
//! `[sources.<id>]` / `[integrations.<id>]` table in `config.toml` (alias)
//! and are attached when the plugin is spawned. Durable identity is
//! [`PluginKey`], not the manifest alias.

use std::path::{Path, PathBuf};

use bookclerk_config::Config;
use bookclerk_library::BOOKCLERK_SCHEMA_NAMESPACE;
use bookclerk_plugin_abi::PRODUCT_API_VERSION;
use bookclerk_plugin_catalog::{evaluate_install_in, PluginInstallIdentity, PluginKey};

use crate::manifest::PluginManifest;
use crate::{PluginError, Result};

/// A discovered plugin ready to spawn.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    /// Parsed `plugin.toml` for this install directory.
    pub manifest: PluginManifest,
    /// Directory containing `plugin.toml` (cwd + relative `command` base).
    pub root: PathBuf,
    /// Absolute path to the plugin executable.
    pub command: PathBuf,
    /// Host-evaluated provenance-qualified identity.
    pub identity: PluginInstallIdentity,
}

impl DiscoveredPlugin {
    /// Builds a discovered plugin, evaluating content hashes and provenance.
    ///
    /// Incomplete synthetic trees should use [`Self::for_test`] instead of
    /// weakening production evaluation.
    ///
    /// # Panics
    ///
    /// Panics when the install tree cannot be evaluated. Prefer [`Self::try_new`].
    #[must_use]
    pub fn new(manifest: PluginManifest, root: PathBuf, command: PathBuf) -> Self {
        Self::try_new(manifest, root, command, None).expect("plugin tree must evaluate")
    }

    /// Production constructor: fail closed on malformed receipts, hash errors,
    /// and unrecognized install receipts.
    ///
    /// # Errors
    ///
    /// Returns when identity evaluation fails closed.
    pub fn try_new(
        manifest: PluginManifest,
        root: PathBuf,
        command: PathBuf,
        files_dir: Option<&Path>,
    ) -> Result<Self> {
        let identity = evaluate_install_in(&root, &manifest, files_dir)
            .map_err(|err| PluginError::message(err.to_string()))?;
        Ok(Self {
            manifest,
            root,
            command,
            identity,
        })
    }

    /// Explicit test/dev constructor for incomplete plugin trees.
    ///
    /// Does not hash the tree or consult receipts. Never grants platform trust.
    #[must_use]
    pub fn for_test(manifest: PluginManifest, root: PathBuf, command: PathBuf) -> Self {
        let identity = local_identity(&root, &manifest);
        Self {
            manifest,
            root,
            command,
            identity,
        }
    }

    /// Display / CLI alias (`plugin.toml` id).
    #[must_use]
    pub fn alias(&self) -> &str {
        self.identity.alias()
    }

    /// Provenance-qualified key.
    #[must_use]
    pub fn plugin_key(&self) -> &PluginKey {
        &self.identity.plugin_key
    }
}

/// Path-only identity used when an install tree cannot be hashed (test fixtures).
fn local_identity(root: &Path, manifest: &PluginManifest) -> PluginInstallIdentity {
    let plugin_key = PluginKey::from_install_path(root, &manifest.id)
        .expect("test plugin root must form a PluginKey");
    PluginInstallIdentity {
        artifact: bookclerk_plugin_catalog::ArtifactIdentity {
            plugin_key: plugin_key.clone(),
            version: manifest.version.clone().unwrap_or_else(|| "0.0.0".into()),
            manifest_sha256: String::new(),
            payload_root_sha256: String::new(),
            archive_sha256: None,
        },
        plugin_key,
        alias: manifest.id.clone(),
        provenance: bookclerk_plugin_catalog::PluginProvenance::LocalDevelopment,
    }
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
/// Duplicate [`PluginKey`] values are a hard error. Duplicate aliases from
/// different PluginKeys are also a hard error (invalid installation state).
///
/// # Errors
///
/// Returns [`PluginError`] on duplicate keys, duplicate aliases, missing binaries, or I/O failures.
pub fn discover_plugins(config: &Config) -> Result<Vec<DiscoveredPlugin>> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for dir in plugin_search_dirs(config) {
        if !dir.is_dir() {
            continue;
        }
        discover_in_dir(
            &dir,
            config.paths().files_dir.as_path(),
            &mut out,
            &mut seen,
        )?;
    }
    assert_unique_aliases(&out)?;
    out.sort_by(|a, b| {
        a.manifest
            .id
            .cmp(&b.manifest.id)
            .then_with(|| a.plugin_key().canonical().cmp(b.plugin_key().canonical()))
    });
    Ok(out)
}

/// Resolves `spec` to a discovered plugin.
///
/// `spec` may be a canonical [`PluginKey`] or an alias unique in this host
/// plugin namespace. In valid installed state an alias maps to exactly one
/// PluginKey. Duplicate aliases fail closed rather than picking first or last.
///
/// # Errors
///
/// Returns an error when `spec` matches nothing, or when corrupt disk state
/// contains duplicate keys or aliases.
pub fn resolve_plugin_ref<'a>(
    plugins: &'a [DiscoveredPlugin],
    spec: &str,
) -> Result<&'a DiscoveredPlugin> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(PluginError::message("plugin reference must not be empty"));
    }
    if let Ok(key) = PluginKey::parse(spec) {
        let matches: Vec<_> = plugins.iter().filter(|p| p.plugin_key() == &key).collect();
        return match matches.as_slice() {
            [one] => Ok(*one),
            [] => Err(PluginError::message(format!(
                "no plugin installed with key `{spec}`"
            ))),
            _ => Err(PluginError::message(format!(
                "duplicate plugin key `{spec}`"
            ))),
        };
    }
    let lower = spec.to_ascii_lowercase();
    let matches: Vec<_> = plugins
        .iter()
        .filter(|p| p.alias().eq_ignore_ascii_case(&lower))
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(PluginError::message(format!(
            "plugin `{spec}` is not installed"
        ))),
        many => {
            let details: Vec<_> = many
                .iter()
                .map(|p| format!("{} at {}", p.plugin_key().canonical(), p.root.display()))
                .collect();
            Err(PluginError::message(format!(
                "duplicate plugin alias `{spec}` is invalid installation state; \
                 remove or repair one of: {}",
                details.join("; ")
            )))
        }
    }
}

/// Discovers `$dir/plugin.toml` or each `$dir/<name>/plugin.toml`; skips unreadable directories.
fn discover_in_dir(
    dir: &Path,
    files_dir: &Path,
    out: &mut Vec<DiscoveredPlugin>,
    seen: &mut std::collections::HashMap<String, PathBuf>,
) -> Result<()> {
    let root_manifest = dir.join("plugin.toml");
    if root_manifest.is_file() {
        push_manifest(&root_manifest, dir, files_dir, out, seen)?;
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
            push_manifest(&manifest_path, &path, files_dir, out, seen)?;
        }
    }
    Ok(())
}

/// Parses a manifest, rejects duplicate keys / missing binaries, and skips newer `api_version`.
fn push_manifest(
    manifest_path: &Path,
    root: &Path,
    files_dir: &Path,
    out: &mut Vec<DiscoveredPlugin>,
    seen: &mut std::collections::HashMap<String, PathBuf>,
) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)?;
    let manifest = PluginManifest::parse(&text)?;
    if manifest.id == BOOKCLERK_SCHEMA_NAMESPACE {
        return Err(PluginError::message(format!(
            "plugin `{}`: id `{BOOKCLERK_SCHEMA_NAMESPACE}` is reserved for the host schema namespace",
            manifest.id
        )));
    }
    if manifest.api_version > PRODUCT_API_VERSION {
        tracing::warn!(
            id = %manifest.id,
            plugin_api = manifest.api_version,
            host_api = PRODUCT_API_VERSION,
            "plugin api_version newer than host; skipping"
        );
        return Ok(());
    }
    let command = resolve_spawn_command(root, &manifest)?;
    if !command.is_file() {
        return Err(PluginError::message(format!(
            "plugin `{}`: command not found at {}",
            manifest.id,
            command.display()
        )));
    }
    let plugin = DiscoveredPlugin::try_new(manifest, root.to_path_buf(), command, Some(files_dir))?;
    let key = plugin.plugin_key().canonical().to_string();
    if let Some(first_path) = seen.get(&key) {
        return Err(PluginError::message(format!(
            "duplicate plugin key `{}` (alias `{}`): already discovered at {} and also at {}",
            key,
            plugin.alias(),
            first_path.display(),
            manifest_path.display()
        )));
    }
    seen.insert(key, manifest_path.to_path_buf());
    out.push(plugin);
    Ok(())
}

/// Fail closed when two PluginKeys share a manifest alias.
fn assert_unique_aliases(plugins: &[DiscoveredPlugin]) -> Result<()> {
    let mut by_alias: std::collections::BTreeMap<String, Vec<&DiscoveredPlugin>> =
        std::collections::BTreeMap::new();
    for plugin in plugins {
        by_alias
            .entry(plugin.alias().to_ascii_lowercase())
            .or_default()
            .push(plugin);
    }
    for (alias, group) in by_alias {
        if group.len() <= 1 {
            continue;
        }
        let details: Vec<_> = group
            .iter()
            .map(|p| format!("{} at {}", p.plugin_key().canonical(), p.root.display()))
            .collect();
        return Err(PluginError::message(format!(
            "duplicate plugin alias `{alias}` is invalid installation state; \
             remove or repair one of: {}",
            details.join("; ")
        )));
    }
    Ok(())
}

/// Resolves the native guest binary or the host `bookclerk-workerd` helper.
fn resolve_spawn_command(root: &Path, manifest: &PluginManifest) -> Result<PathBuf> {
    use crate::manifest::PluginRuntimeKind;
    match manifest.runtime {
        PluginRuntimeKind::Native => {
            let command = manifest.command.as_ref().ok_or_else(|| {
                PluginError::message(format!(
                    "plugin `{}`: native runtime missing command",
                    manifest.id
                ))
            })?;
            resolve_command(root, command)
        }
        // Discovery only records where the launcher is; the pinned `workerd`
        // beside it is checked when the spawn plan resolves.
        PluginRuntimeKind::Workerd => crate::spawn_plan::locate_launcher(),
    }
}

/// Treats relative `command` as rooted at the plugin install directory.
fn resolve_command(root: &Path, command: &Path) -> Result<PathBuf> {
    if command.is_absolute() {
        return Ok(command.to_path_buf());
    }
    Ok(root.join(command))
}

/// Opaque knobs from main `config.toml` for this plugin id (by primary family).
#[must_use]
pub fn settings_table(config: &Config, plugin: &DiscoveredPlugin) -> toml::Table {
    settings_table_for(config, plugin, plugin.manifest.primary_family())
}

/// Resolves the `config.toml` table for `plugin` under one specific handler
/// `family`.
///
/// A plugin exporting several entrypoints (for example `storefront` + an
/// event consumer) owns one settings table per family (`[sources.<id>]` and
/// `[integrations.<id>]`); callers rendering per-family settings groups pick
/// the family explicitly instead of relying on [`PluginManifest::primary_family`].
///
/// [`PluginManifest::primary_family`]: crate::PluginManifest::primary_family
pub fn settings_table_for(
    config: &Config,
    plugin: &DiscoveredPlugin,
    family: crate::PluginFamily,
) -> toml::Table {
    match family {
        crate::PluginFamily::Source => config
            .sources
            .table(&plugin.manifest.id)
            .cloned()
            .unwrap_or_default(),
        crate::PluginFamily::Integration => {
            let mut table = config
                .integrations
                .plugin_table(&plugin.manifest.id)
                .cloned()
                .unwrap_or_default();
            inject_abs_api_key_from_env(&plugin.manifest.id, &mut table);
            table
        }
        crate::PluginFamily::Output if plugin.manifest.id == "s3" => {
            output_s3_settings_table(&config.output.s3)
        }
        crate::PluginFamily::Output if plugin.manifest.id == "local" => {
            output_local_settings_table(&config.output.local)
        }
        crate::PluginFamily::Database => database_settings_table(config, plugin),
        crate::PluginFamily::Output => toml::Table::new(),
    }
}

/// Serializes `[database.<id>]` for the matching database plugin id.
///
/// First-party ids use their typed config sections; third-party adapters get
/// the opaque `[database.<id>]` table (delivered as `DatabaseAdapterConfig`).
fn database_settings_table(config: &Config, plugin: &DiscoveredPlugin) -> toml::Table {
    let id = plugin.manifest.id.to_ascii_lowercase();
    let value = match id.as_str() {
        "sqlite" => toml::Value::try_from(&config.database.sqlite),
        "d1" => toml::Value::try_from(&config.database.d1),
        "postgres" => toml::Value::try_from(&config.database.postgres),
        _ => {
            return config
                .database
                .plugin_table(&plugin.manifest.id)
                .or_else(|| config.database.plugin_table(&id))
                .cloned()
                .unwrap_or_default();
        }
    };
    match value {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

/// Serializes `[output.s3]` into the spawn settings table.
fn output_s3_settings_table(cfg: &bookclerk_config::OutputS3Config) -> toml::Table {
    match toml::Value::try_from(cfg) {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

/// Serializes `[output.local]` into the spawn settings table.
fn output_local_settings_table(cfg: &bookclerk_config::OutputLocalConfig) -> toml::Table {
    match toml::Value::try_from(cfg) {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

/// When ABS config lacks `api_key`, inject `BOOKCLERK_ABS_API_KEY` into the
/// spawn config table (plugin processes do not inherit Bookclerk env secrets).
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
api_version = 3
id = "echo"
runtime = "native"
command = "./echo-bin"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
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

    fn write_plugin(dir: &Path, id: &str, entrypoint: &str) {
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
api_version = 3
id = "{id}"
runtime = "native"
command = "./bin"
entrypoints = ["{entrypoint}"]

[capabilities.network]
mode = "deny"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    /// Invalid installation state: two PluginKeys sharing alias `echo` fail closed.
    fn duplicate_aliases_fail_closed_with_both_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo-a"), "echo", "cli");
        write_plugin(&plugins.join("echo-b"), "echo", "cli");

        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let err = discover_plugins(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate plugin alias"), "{err}");
        assert!(err.contains("invalid installation state"), "{err}");
        assert!(err.contains("echo-a") || err.contains("path:"), "{err}");
    }

    #[test]
    /// Invalid installation state: the same alias cannot be a source and an integration.
    fn same_alias_different_family_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo-src"), "echo", "storefront");
        write_plugin(&plugins.join("echo-int"), "echo", "cli");

        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let err = discover_plugins(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate plugin alias"), "{err}");
    }

    #[test]
    fn bare_alias_resolves_exactly_one_plugin_key() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo"), "echo", "cli");
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let found = discover_plugins(&cfg).unwrap();
        assert_eq!(found.len(), 1);
        let by_alias = resolve_plugin_ref(&found, "echo").unwrap();
        let by_key = resolve_plugin_ref(&found, found[0].plugin_key().canonical()).unwrap();
        assert_eq!(by_alias.plugin_key(), by_key.plugin_key());
    }

    #[test]
    fn reserved_bookclerk_plugin_id_is_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("host"), "bookclerk", "cli");
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let err = discover_plugins(&cfg).unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
        assert!(err.contains("bookclerk"), "{err}");
    }

    #[test]
    fn leftover_migration_plan_toml_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        let nested = plugins.join("echo_sql");
        write_plugin(&nested, "echo_sql", "cli");
        fs::write(
            nested.join("plugin.toml"),
            r#"
api_version = 3
id = "echo_sql"
runtime = "native"
command = "./bin"
migration_plan = "migrations.toml"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[databases]]
binding = "DB"
"#,
        )
        .unwrap();
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let err = discover_plugins(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("migration_plan") || err.to_lowercase().contains("unknown"),
            "{err}"
        );
    }

    #[test]
    fn malformed_receipt_fails_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo"), "echo", "cli");
        fs::write(plugins.join("echo").join("receipt.json"), b"{not-json").unwrap();
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let err = discover_plugins(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("receipt")
                || err.contains("malformed")
                || err.contains("json")
                || err.contains("expected")
                || err.contains("string"),
            "{err}"
        );
    }

    #[test]
    fn absent_receipt_is_local_development() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("echo"), "echo", "cli");
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let found = discover_plugins(&cfg).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].identity.provenance,
            bookclerk_plugin_catalog::PluginProvenance::LocalDevelopment
        );
    }
}
