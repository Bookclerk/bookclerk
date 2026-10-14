//! Scan plugin directories for `plugin.toml` manifests.
//!
//! Discovery is install-time only. User settings come from the matching
//! `[sources.<id>]` / `[integrations.<id>]` table in `config.toml` (alias)
//! and are attached when the plugin is spawned. Durable identity is
//! [`PluginKey`], not the manifest alias.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use bookclerk_config::{Config, DatabasePluginKind};
use bookclerk_library::BOOKCLERK_SCHEMA_NAMESPACE;
use bookclerk_plugin_abi::PRODUCT_API_VERSION;
use bookclerk_plugin_catalog::{evaluate_install_in, PluginInstallIdentity, PluginKey};

use crate::manifest::{PluginFamily, PluginManifest};
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
    /// Consent grant recorded on the cluster release this tree was
    /// materialized from; `None` for host-local trees, whose grant lives in
    /// `plugin-grants.json`.
    pub release_grant: Option<crate::PluginGrant>,
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
            release_grant: None,
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
            release_grant: None,
        }
    }

    /// Builds a plugin from a tree materialized from a cluster release.
    ///
    /// The caller has already verified the tree's payload-root digest
    /// against the release row; identity, version, and the consent grant
    /// come from that row, not from a receipt or the host grant file.
    ///
    /// # Errors
    ///
    /// Returns an error when the spawn command cannot be resolved, the
    /// manifest cannot be hashed, or `plugin_key` is malformed.
    pub fn from_release(
        manifest: PluginManifest,
        root: PathBuf,
        plugin_key: &str,
        version: &str,
        payload_root_sha256: &str,
        archive_sha256: &str,
        grant: crate::PluginGrant,
    ) -> Result<Self> {
        let key =
            PluginKey::parse(plugin_key).map_err(|err| PluginError::message(err.to_string()))?;
        let command = resolve_spawn_command(&root, &manifest)?;
        let manifest_sha256 = bookclerk_plugin_catalog::manifest_sha256(&root)
            .map_err(|err| PluginError::message(err.to_string()))?;
        let identity = PluginInstallIdentity {
            plugin_key: key.clone(),
            alias: manifest.id.clone(),
            artifact: bookclerk_plugin_catalog::ArtifactIdentity {
                plugin_key: key,
                version: version.to_string(),
                manifest_sha256,
                payload_root_sha256: payload_root_sha256.to_string(),
                archive_sha256: Some(archive_sha256.to_string()),
            },
            provenance: bookclerk_plugin_catalog::PluginProvenance::Released,
        };
        Ok(Self {
            manifest,
            root,
            command,
            identity,
            release_grant: Some(grant),
        })
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

/// First-party database adapter kind when provenance + PluginKey match.
///
/// `PlatformBundled` (platform sqlite) and `VerifiedInstalled` (crates.io /
/// platform postgres and D1) both qualify. `LocalDevelopment` and `Modified`
/// never do. Alias text (`id = "sqlite"`) is not consulted.
#[must_use]
pub fn first_party_database_kind(plugin: &DiscoveredPlugin) -> Option<DatabasePluginKind> {
    if !plugin.identity.provenance.is_verified_artifact() {
        return None;
    }
    if !bookclerk_plugin_catalog::is_first_party_database_adapter(plugin.plugin_key()) {
        return None;
    }
    bookclerk_plugin_catalog::FIRST_PARTY_DATABASE_ADAPTERS
        .iter()
        .find(|artifact| artifact.package_name == plugin.plugin_key().package())
        .and_then(|artifact| DatabasePluginKind::parse(artifact.manifest_id))
}

/// True when this install is the verified Bookclerk local-filesystem destination.
#[must_use]
pub fn is_first_party_local_output(plugin: &DiscoveredPlugin) -> bool {
    plugin.identity.provenance.is_verified_artifact()
        && bookclerk_plugin_catalog::is_first_party_destination(plugin.plugin_key())
        && plugin.plugin_key().package() == "bookclerk-plugin-destination-local"
}

/// True when this install is the verified Bookclerk S3 destination.
#[must_use]
pub fn is_first_party_s3_output(plugin: &DiscoveredPlugin) -> bool {
    plugin.identity.provenance.is_verified_artifact()
        && bookclerk_plugin_catalog::is_first_party_destination(plugin.plugin_key())
        && plugin.plugin_key().package() == "bookclerk-plugin-destination-s3"
}

/// True when this install is the verified Bookclerk Audiobookshelf integration.
///
/// Only this plugin receives the host-implied network grant for the server
/// its instance document names (`base_url`). The live manifest alias is not
/// consulted: an alias twin is a different PluginKey with its own consent.
#[must_use]
pub fn is_first_party_audiobookshelf(plugin: &DiscoveredPlugin) -> bool {
    plugin.identity.provenance.is_verified_artifact()
        && bookclerk_plugin_catalog::is_first_party_integration(plugin.plugin_key())
        && plugin.plugin_key().package() == "bookclerk-plugin-integration-audiobookshelf"
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

// Counts `discover_plugins` calls inside one test task. Other tests do not
// increment it. A process-wide counter raced with them.
#[cfg(test)]
tokio::task_local! {
    pub static DISCOVER_CALLS: std::sync::Arc<AtomicUsize>;
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
    #[cfg(test)]
    {
        let _ = DISCOVER_CALLS.try_with(|calls| calls.fetch_add(1, Ordering::Relaxed));
    }
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

/// Occupancy identities (alias + PluginKey) **without** payload hashing.
///
/// Host spawn overlays only need to uniquify occupancy and fail closed on
/// corrupt duplicate aliases. Re-running [`discover_plugins`] here would
/// SHA-256 every staged debug binary on each spawn (hundreds of MiB each)
/// and stall CI. Provenance for the guest being spawned stays on the
/// already-evaluated [`DiscoveredPlugin`] passed into spawn.
///
/// # Errors
///
/// Returns [`PluginError`] on duplicate keys, duplicate aliases, or I/O
/// failures while reading manifests.
pub(crate) fn discover_occupancy_plugins(config: &Config) -> Result<Vec<DiscoveredPlugin>> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for dir in plugin_search_dirs(config) {
        if !dir.is_dir() {
            continue;
        }
        occupancy_in_dir(&dir, &mut out, &mut seen)?;
    }
    assert_unique_aliases(&out)?;
    Ok(out)
}

/// Config occupancy selector (`plugin = "…"`), or `alias` when the field is empty.
///
/// Occupancy is how host config names the guest that may use a singleton
/// slot (`[database].plugin`, `[output.s3].plugin`, `[sources.<id>].plugin`).
/// An empty field still means the display alias, which
/// [`resolve_plugin_slot`] accepts only when exactly one install uses it.
///
/// # Arguments
///
/// * `plugin_field` - Occupancy string from config (`plugin = "…"`).
/// * `alias` - Display alias used when `plugin_field` is empty.
#[must_use]
pub fn occupancy_spec<'a>(plugin_field: &'a str, alias: &'a str) -> &'a str {
    let spec = plugin_field.trim();
    if spec.is_empty() {
        alias
    } else {
        spec
    }
}

/// True when `plugin_key` / `alias` is the occupant named by `spec`.
///
/// A parseable PluginKey never falls through to an alias comparison, so a
/// miss cannot inherit another provenance's grant or session. Does not
/// detect corrupt duplicate aliases — callers that load or privilege-check
/// must require a unique match ([`resolve_plugin_slot`]).
///
/// # Arguments
///
/// * `plugin_key` - Canonical [`PluginKey`] text.
/// * `alias` - Manifest display id.
/// * `spec` - Occupancy selector from [`occupancy_spec`] or a job/CLI id.
#[must_use]
pub fn identity_matches_occupancy(plugin_key: &str, alias: &str, spec: &str) -> bool {
    let spec = spec.trim();
    if spec.is_empty() {
        return false;
    }
    if let Ok(key) = PluginKey::parse(spec) {
        return plugin_key == key.canonical();
    }
    alias.eq_ignore_ascii_case(spec)
}

/// True when `plugin` is the occupant named by `spec` (PluginKey or alias).
///
/// Does not detect corrupt duplicate aliases — loaders must call [`resolve_plugin_slot`].
///
/// # Arguments
///
/// * `plugin` - Candidate install.
/// * `spec` - Occupancy selector from [`occupancy_spec`].
#[must_use]
pub fn plugin_matches_occupancy(plugin: &DiscoveredPlugin, spec: &str) -> bool {
    identity_matches_occupancy(plugin.plugin_key().canonical(), plugin.alias(), spec)
}

/// Resolves `spec` among `plugins` without treating a vacant slot as an error.
///
/// `None` means nothing installed matches. An ambiguous alias is an error
/// so callers fail closed instead of spawning every twin or last-write-wins.
///
/// # Arguments
///
/// * `plugins` - Already-filtered family candidates.
/// * `spec` - Occupancy selector from [`occupancy_spec`].
///
/// # Errors
///
/// Returns [`PluginError`] when two installs share the alias (or PluginKey).
pub fn resolve_plugin_slot<'a>(
    plugins: &'a [DiscoveredPlugin],
    spec: &str,
) -> Result<Option<&'a DiscoveredPlugin>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    if let Ok(key) = PluginKey::parse(spec) {
        let matches: Vec<_> = plugins.iter().filter(|p| p.plugin_key() == &key).collect();
        return match matches.as_slice() {
            [one] => Ok(Some(*one)),
            [] => Ok(None),
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
        [one] => Ok(Some(*one)),
        [] => Ok(None),
        many => {
            let keys: Vec<_> = many.iter().map(|p| p.plugin_key().canonical()).collect();
            Err(PluginError::message(format!(
                "duplicate plugin alias `{spec}` is invalid installation state; \
                 remove or repair one of: {}",
                keys.join(", ")
            )))
        }
    }
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
    match resolve_plugin_slot(plugins, spec)? {
        Some(plugin) => Ok(plugin),
        None if PluginKey::parse(spec).is_ok() => Err(PluginError::message(format!(
            "no plugin installed with key `{spec}`"
        ))),
        None => Err(PluginError::message(format!(
            "plugin `{spec}` is not installed"
        ))),
    }
}

/// True when occupancy `spec` names the display alias (or kind token) `alias`.
///
/// Bare kind tokens (`pg` ↔ `postgres`) match without discovery. A PluginKey
/// occupant matches only when it is a host-controlled first-party package
/// whose **package name** maps to that alias — never via a `#fragment`.
/// Third-party keys need [`occupancy_names_alias`] plus discovery.
///
/// A bare alias does **not** match a PluginKey string passed as `alias`.
///
/// # Arguments
///
/// * `spec` - Occupancy selector from config (`plugin = "…"`).
/// * `alias` - Display alias or first-party kind token being compared.
#[must_use]
pub fn occupancy_matches_alias(spec: &str, alias: &str) -> bool {
    let spec = spec.trim();
    let alias = alias.trim();
    if spec.is_empty() || alias.is_empty() {
        return false;
    }
    if spec.eq_ignore_ascii_case(alias) {
        return true;
    }
    if let Ok(key) = PluginKey::parse(spec) {
        if PluginKey::parse(alias).is_ok() {
            return false;
        }
        return first_party_occupancy_alias(&key).is_some_and(|id| kind_or_alias_match(id, alias));
    }
    if PluginKey::parse(alias).is_ok() {
        return false;
    }
    kind_or_alias_match(spec, alias)
}

/// True when occupancy `spec` names `alias` among `discovered` installs.
///
/// Uses [`occupancy_matches_alias`] first, then resolves `spec` to exactly one
/// PluginKey. Corrupt duplicate aliases fail closed (`false`).
#[must_use]
pub fn occupancy_names_alias(spec: &str, alias: &str, discovered: &[DiscoveredPlugin]) -> bool {
    if occupancy_matches_alias(spec, alias) {
        return true;
    }
    match resolve_plugin_slot(discovered, spec) {
        Ok(Some(plugin)) => kind_or_alias_match(plugin.alias(), alias),
        _ => false,
    }
}

/// Host-controlled alias for a first-party PluginKey (package + scheme, not fragment).
fn first_party_occupancy_alias(key: &PluginKey) -> Option<&'static str> {
    if bookclerk_plugin_catalog::is_first_party_database_adapter(key) {
        return bookclerk_plugin_catalog::FIRST_PARTY_DATABASE_ADAPTERS
            .iter()
            .find(|artifact| artifact.package_name == key.package())
            .map(|artifact| artifact.manifest_id);
    }
    if bookclerk_plugin_catalog::is_first_party_destination(key) {
        return bookclerk_plugin_catalog::FIRST_PARTY_DESTINATIONS
            .iter()
            .find(|artifact| artifact.package_name == key.package())
            .map(|artifact| artifact.manifest_id);
    }
    None
}

/// Case-insensitive alias or first-party kind-token equality (`pg` ↔ `postgres`).
fn kind_or_alias_match(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.eq_ignore_ascii_case(right) {
        return true;
    }
    let left_kind = DatabasePluginKind::parse(left);
    let right_kind = DatabasePluginKind::parse(right);
    left_kind.is_some() && left_kind == right_kind
}

/// Records `plugin` as the `[database].plugin` occupant.
///
/// Database adapters are the bootstrap exception: they stay selected in
/// `config.toml` because the connection is needed before the database can be
/// read. Every other plugin is enabled by a `plugin_deployments` row.
///
/// # Errors
///
/// Returns an error when `plugin` is not a database adapter.
pub fn stamp_occupancy_plugin_key(config: &mut Config, plugin: &DiscoveredPlugin) -> Result<()> {
    if !plugin.manifest.families().contains(&PluginFamily::Database) {
        return Err(PluginError::message(format!(
            "plugin `{}` is enabled per host with `bookclerk plugins enable`, not in config.toml",
            plugin.alias()
        )));
    }
    config.database.plugin = plugin.plugin_key().canonical().to_string();
    Ok(())
}

/// Rewrites a unique `[database].plugin` alias to the canonical PluginKey.
///
/// An ambiguous alias or vacant slot is left unchanged so loaders still fail
/// closed. An empty `discovered` slice is a no-op so a settings discovery
/// timeout cannot wipe occupancy.
pub fn upgrade_unique_alias_occupancy(config: &mut Config, discovered: &[DiscoveredPlugin]) {
    if discovered.is_empty() {
        return;
    }
    upgrade_occupancy_field(&mut config.database.plugin, discovered);
}

/// Canonical PluginKey when `spec` uniquely resolves and is not already that key.
fn unique_canonical_occupancy(spec: &str, discovered: &[DiscoveredPlugin]) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    match resolve_plugin_slot(discovered, spec) {
        Ok(Some(plugin)) => {
            let canonical = plugin.plugin_key().canonical();
            if spec == canonical {
                None
            } else {
                Some(canonical.to_string())
            }
        }
        _ => None,
    }
}

/// Replaces `field` with a unique PluginKey when `spec` is a unique alias.
fn upgrade_occupancy_field(field: &mut String, discovered: &[DiscoveredPlugin]) {
    if let Some(key) = unique_canonical_occupancy(field, discovered) {
        *field = key;
    }
}

/// Occupancy walk of `$dir/plugin.toml` or `$dir/<name>/plugin.toml` (no payload hash).
fn occupancy_in_dir(
    dir: &Path,
    out: &mut Vec<DiscoveredPlugin>,
    seen: &mut std::collections::HashMap<String, PathBuf>,
) -> Result<()> {
    let root_manifest = dir.join("plugin.toml");
    if root_manifest.is_file() {
        push_occupancy_manifest(&root_manifest, dir, out, seen)?;
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
            push_occupancy_manifest(&manifest_path, &path, out, seen)?;
        }
    }
    Ok(())
}

/// Parses a manifest for occupancy (PluginKey + alias) without hashing the tree.
fn push_occupancy_manifest(
    manifest_path: &Path,
    root: &Path,
    out: &mut Vec<DiscoveredPlugin>,
    seen: &mut std::collections::HashMap<String, PathBuf>,
) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)?;
    let manifest = PluginManifest::parse(&text)?;
    if manifest.id == BOOKCLERK_SCHEMA_NAMESPACE {
        return Ok(());
    }
    if manifest.api_version > PRODUCT_API_VERSION {
        return Ok(());
    }
    let command = match resolve_spawn_command(root, &manifest) {
        Ok(command) => command,
        Err(_) => root.to_path_buf(),
    };
    let plugin = DiscoveredPlugin::for_test(manifest, root.to_path_buf(), command);
    let key = plugin.plugin_key().canonical().to_string();
    if let Some(first_path) = seen.get(&key) {
        return Err(PluginError::message(format!(
            "duplicate plugin key `{key}` (alias `{}`): already discovered at {} and also at {}",
            plugin.alias(),
            first_path.display(),
            manifest_path.display()
        )));
    }
    seen.insert(key, manifest_path.to_path_buf());
    out.push(plugin);
    Ok(())
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

/// Bootstrap connection settings for a database adapter from `config.toml`.
///
/// Database adapters are the one plugin family whose configuration stays in
/// the bootstrap file: the process needs it to open the library before the
/// database-backed control plane exists. Every other family receives its
/// settings from the plugin instance document.
#[must_use]
pub fn settings_table(config: &Config, plugin: &DiscoveredPlugin) -> toml::Table {
    settings_table_for(config, plugin, plugin.manifest.primary_family())
}

/// [`settings_table`] for one explicit handler `family`.
///
/// Only [`PluginFamily::Database`](crate::PluginFamily::Database) has a
/// `config.toml` table; every other family returns an empty table.
pub fn settings_table_for(
    config: &Config,
    plugin: &DiscoveredPlugin,
    family: crate::PluginFamily,
) -> toml::Table {
    match family {
        crate::PluginFamily::Database => database_settings_table(config, plugin),
        crate::PluginFamily::Source
        | crate::PluginFamily::Integration
        | crate::PluginFamily::Output => toml::Table::new(),
    }
}

/// Serializes `[database.<id>]` for the matching database plugin id.
///
/// First-party verified adapters use their typed config sections; third-party
/// adapters (including trees that reuse the `sqlite` / `postgres` / `d1`
/// alias) get the opaque `[database.<id>]` table.
fn database_settings_table(config: &Config, plugin: &DiscoveredPlugin) -> toml::Table {
    let value = match first_party_database_kind(plugin) {
        Some(DatabasePluginKind::Sqlite) => toml::Value::try_from(&config.database.sqlite),
        Some(DatabasePluginKind::D1) => toml::Value::try_from(&config.database.d1),
        Some(DatabasePluginKind::Postgres) => toml::Value::try_from(&config.database.postgres),
        None => {
            return config
                .database
                .plugin_table(&plugin.manifest.id)
                .or_else(|| {
                    config
                        .database
                        .plugin_table(&plugin.manifest.id.to_ascii_lowercase())
                })
                .cloned()
                .unwrap_or_default();
        }
    };
    match value {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Sets the Unix execute bit so discovery tests can treat the stub as a command.
    fn chmod_exec(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    #[test]
    fn discovers_nested_plugin_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let nested = plugins_root.join("echo");
        fs::create_dir_all(&nested).unwrap();
        let bin = nested.join("echo-bin");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        chmod_exec(&bin);
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

        // Non-database plugins have no `config.toml` table: their settings
        // live in the instance document.
        let settings = settings_table(&cfg, &found[0]);
        assert!(settings.is_empty(), "{settings:?}");
    }

    fn write_plugin(dir: &Path, id: &str, entrypoint: &str) {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join("bin");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        chmod_exec(&bin);
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
    fn occupancy_spec_prefers_plugin_key_over_alias() {
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
        assert_eq!(occupancy_spec("", "s3"), "s3");
        assert_eq!(occupancy_spec("  ", "s3"), "s3");
        assert_eq!(
            occupancy_spec(found[0].plugin_key().canonical(), "s3"),
            found[0].plugin_key().canonical()
        );
        assert!(plugin_matches_occupancy(
            &found[0],
            found[0].plugin_key().canonical()
        ));
        assert!(plugin_matches_occupancy(&found[0], "echo"));
        assert!(!plugin_matches_occupancy(&found[0], "other"));
        assert!(identity_matches_occupancy(
            found[0].plugin_key().canonical(),
            found[0].alias(),
            found[0].plugin_key().canonical()
        ));
        assert!(identity_matches_occupancy(
            found[0].plugin_key().canonical(),
            "echo",
            "ECHO"
        ));
        assert!(!identity_matches_occupancy(
            found[0].plugin_key().canonical(),
            "echo",
            ""
        ));
        assert!(!identity_matches_occupancy(
            found[0].plugin_key().canonical(),
            "echo",
            "other"
        ));
        assert!(!occupancy_matches_alias(
            found[0].plugin_key().canonical(),
            "echo"
        ));
        assert!(occupancy_names_alias(
            found[0].plugin_key().canonical(),
            "echo",
            &found
        ));
        assert!(!occupancy_matches_alias(
            "echo",
            found[0].plugin_key().canonical()
        ));
    }

    #[test]
    fn occupancy_matches_alias_kind_tokens_and_keys() {
        assert!(occupancy_matches_alias("sqlite", "sqlite"));
        assert!(occupancy_matches_alias("pg", "postgres"));
        assert!(occupancy_matches_alias("postgres", "postgresql"));
        assert!(occupancy_matches_alias(
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite"
        ));
        assert!(occupancy_matches_alias(
            "platform:bookclerk/bookclerk-plugin-database-postgres",
            "pg"
        ));
        assert!(!occupancy_matches_alias(
            "sqlite",
            "platform:bookclerk/bookclerk-plugin-database-sqlite"
        ));
        assert!(!occupancy_matches_alias(
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "postgres"
        ));
        assert!(!occupancy_matches_alias(
            "cargo:https://evil.example#bookclerk-plugin-database-postgres",
            "postgres"
        ));
        assert!(!occupancy_matches_alias("", "sqlite"));
    }

    #[test]
    fn stamp_and_upgrade_unique_occupancy_to_plugin_key() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("sqlite"), "sqlite", "databaseAdapter");
        write_plugin(&plugins.join("echo"), "echo", "cli");
        write_plugin(&plugins.join("s3"), "s3", "storage");

        let mut cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let found = discover_plugins(&cfg).unwrap();
        let sqlite = resolve_plugin_ref(&found, "sqlite").unwrap();
        stamp_occupancy_plugin_key(&mut cfg, sqlite).unwrap();
        assert_eq!(cfg.database.plugin, sqlite.plugin_key().canonical());

        let echo = resolve_plugin_ref(&found, "echo").unwrap();
        let err = stamp_occupancy_plugin_key(&mut cfg, echo)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("plugins enable"),
            "non-database plugins are deployed, not stamped into config.toml: {err}"
        );
        let s3 = resolve_plugin_ref(&found, "s3").unwrap();
        assert!(stamp_occupancy_plugin_key(&mut cfg, s3).is_err());

        cfg.database.plugin = "sqlite".into();
        upgrade_unique_alias_occupancy(&mut cfg, &found);
        assert_eq!(cfg.database.plugin, sqlite.plugin_key().canonical());

        upgrade_unique_alias_occupancy(&mut cfg, &[]);
        assert_eq!(cfg.database.plugin, sqlite.plugin_key().canonical());
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
    fn occupancy_scan_lists_aliases_without_payload_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("sqlite"), "sqlite", "databaseAdapter");
        write_plugin(&plugins.join("echo"), "echo", "cli");
        std::fs::write(plugins.join("sqlite").join("blob.bin"), vec![0_u8; 256]).unwrap();
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let occ = discover_occupancy_plugins(&cfg).expect("occupancy");
        let aliases: Vec<_> = occ.iter().map(|p| p.alias().to_string()).collect();
        assert!(aliases.iter().any(|a| a == "sqlite"), "{aliases:?}");
        assert!(aliases.iter().any(|a| a == "echo"), "{aliases:?}");
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

    #[test]
    fn third_party_aliases_do_not_inherit_first_party_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        write_plugin(&plugins.join("sqlite"), "sqlite", "databaseAdapter");
        write_plugin(&plugins.join("postgres"), "postgres", "databaseAdapter");
        write_plugin(&plugins.join("d1"), "d1", "databaseAdapter");
        write_plugin(&plugins.join("local"), "local", "storage");
        let mut cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        cfg.database.sqlite.path = Some(std::path::PathBuf::from("secret-library.db"));
        cfg.database.postgres.url = Some("postgres://secret@db/library".into());
        cfg.database.d1.account_id = "acct_secret".into();
        let found = discover_plugins(&cfg).unwrap();
        for id in ["sqlite", "postgres", "d1", "local"] {
            let plugin = found.iter().find(|p| p.alias() == id).expect(id);
            assert!(
                first_party_database_kind(plugin).is_none(),
                "{id} must not be first-party from a path install"
            );
            assert!(
                !is_first_party_local_output(plugin),
                "{id} must not inherit local output privilege"
            );
            assert!(
                !is_first_party_s3_output(plugin),
                "{id} must not inherit s3 privilege"
            );
            let table = settings_table(&cfg, plugin);
            let dump = toml::to_string(&toml::Value::Table(table)).unwrap();
            assert!(
                !dump.contains("secret") && !dump.contains("acct_secret"),
                "{id} received first-party settings: {dump}"
            );
        }
    }
}
