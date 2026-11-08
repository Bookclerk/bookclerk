//! Directory-driven discovery, build, stage, and platform install for guests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use bookclerk_plugin_catalog::{
    platform_artifact, PackageCoordinate, PluginKey, PluginMutationLock, RegistrySource,
    CRATES_IO_INDEX,
};

/// Workspace-relative directory of always-shipped platform guests (`sqlite`, `local`).
const PLATFORM_PLUGINS_DIR: &str = "crates/bookclerk-plugins/platform";
/// Workspace-relative directory of optional storefront and database guests.
const OPTIONAL_PLUGINS_DIR: &str = "crates/bookclerk-plugins/optional";
/// Workspace-relative directory of CI/dev-only Echo example guests.
const EXAMPLES_DIR: &str = "examples";

/// Helper binaries that ship beside hosts (also listed in workspace `default-members`).
pub const HELPER_PACKAGES: &[&str] = &[
    "bookclerk-jail",
    "bookclerk-workerd",
    "bookclerk-media-worker",
];

/// Which guest trees (and installer stack) a build/stage should include.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildSelection {
    /// Workspace `default-members` + platform guests under `crates/bookclerk-plugins/platform`.
    pub platform: bool,
    /// Guests under `crates/bookclerk-plugins/optional`.
    pub optional: bool,
    /// Guests under `examples`.
    pub examples: bool,
}

/// One discovered guest (platform, optional, or example).
#[derive(Debug, Clone)]
pub struct DiscoveredGuest {
    /// Stable identifier for this item.
    pub id: String,
    /// Discriminant or category for this value.
    pub kind: String,
    /// Plugin source or install directory relative to the workspace.
    pub dir: PathBuf,
    /// Relative to workspace root (for packaging / logs).
    pub rel_dir: String,
    /// Cargo / npm package name for this plugin guest.
    pub package: Option<String>,
    /// Compiled binary name when the guest is a native executable.
    pub bin_name: Option<String>,
    /// Expected `plugin.toml` package name / id.
    pub manifest_name: String,
    /// Guest runtime (`native`, `workerd`, `python`, …).
    pub runtime: String,
}

/// Resolve Cargo `-p` names for [`BuildSelection`].
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `sel` - `sel` input for this call.
///
/// # Returns
///
/// On success, the inner `Vec<String>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn packages_for(root: &Path, sel: BuildSelection) -> Result<Vec<String>> {
    let mut pkgs = Vec::new();
    if sel.platform {
        for pkg in default_members(root)? {
            push_unique(&mut pkgs, pkg);
        }
        for guest in discover_tier(root, PLATFORM_PLUGINS_DIR)? {
            push_native_package(&mut pkgs, &guest);
        }
    }
    if sel.optional {
        for guest in discover_tier(root, OPTIONAL_PLUGINS_DIR)? {
            push_native_package(&mut pkgs, &guest);
        }
    }
    if sel.examples {
        for guest in discover_examples(root)? {
            push_native_package(&mut pkgs, &guest);
        }
    }
    Ok(pkgs)
}

/// Resolves plugin manifest ids to discovered guests across every tier.
///
/// Lets CI and developers build or stage one guest (for example `libro`)
/// without the rest of its tier.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `ids` - Plugin manifest ids (`plugin.toml` `id`); duplicates are ignored.
///
/// # Returns
///
/// Guests in the order first requested.
///
/// # Errors
///
/// Returns an error naming every unknown id, or when discovery fails.
pub fn guests_by_id(root: &Path, ids: &[String]) -> Result<Vec<DiscoveredGuest>> {
    let mut all = discover_platform(root)?;
    all.extend(discover_optional(root)?);
    all.extend(discover_examples(root)?);
    let mut out: Vec<DiscoveredGuest> = Vec::new();
    let mut unknown = Vec::new();
    for id in ids {
        if out.iter().any(|g| &g.id == id) {
            continue;
        }
        match all.iter().find(|g| &g.id == id) {
            Some(guest) => out.push(guest.clone()),
            None => unknown.push(id.as_str()),
        }
    }
    if !unknown.is_empty() {
        let mut known: Vec<_> = all.iter().map(|g| g.id.as_str()).collect();
        known.sort_unstable();
        bail!("unknown plugin id(s) {unknown:?}; known: {known:?}");
    }
    Ok(out)
}

/// Cargo `-p` names needed to build `guests` (workerd guests ship `modules/`).
///
/// # Arguments
///
/// * `guests` - Guests resolved by [`guests_by_id`] or tier discovery.
///
/// # Returns
///
/// Unique native package names in input order.
pub fn packages_for_guests(guests: &[DiscoveredGuest]) -> Vec<String> {
    let mut pkgs = Vec::new();
    for guest in guests {
        push_native_package(&mut pkgs, guest);
    }
    pkgs
}

/// Whether `guest` is a platform guest (installed, never staged).
fn is_platform_guest(guest: &DiscoveredGuest) -> bool {
    guest.rel_dir.starts_with(PLATFORM_PLUGINS_DIR)
}

/// Canonical PluginKey used when staging/installing `guest`.
///
/// Platform artifacts use `platform:bookclerk/{package}`. Workspace
/// Cargo packages use the crates.io `cargo:` form so dest leaves stay stable.
/// Other trees use a path key of the source directory.
///
/// # Errors
///
/// Returns when the id or coordinate cannot form a PluginKey.
pub fn guest_plugin_key(guest: &DiscoveredGuest) -> Result<PluginKey> {
    if let Some(package) = guest.package.as_deref() {
        if platform_artifact(package, &guest.id).is_some() {
            return PluginKey::platform(package, &guest.id)
                .map_err(|err| anyhow::anyhow!(err.to_string()));
        }
        let coordinate = PackageCoordinate {
            source: RegistrySource::Cargo {
                registry_url: CRATES_IO_INDEX.to_string(),
            },
            name: package.to_string(),
            version: "0.0.0".into(),
        };
        return PluginKey::from_coordinate(&coordinate, &guest.id)
            .map_err(|err| anyhow::anyhow!(err.to_string()));
    }
    PluginKey::from_install_path(&guest.dir, &guest.id)
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// Install-directory leaf (`pk-` + 128-bit digest) for `guest`.
///
/// # Errors
///
/// Returns when [`guest_plugin_key`] fails.
pub fn guest_install_leaf(guest: &DiscoveredGuest) -> Result<String> {
    Ok(guest_plugin_key(guest)?.fs_id())
}

/// Native guests contribute a Cargo package; workerd guests ship `modules/`
/// (Wasm crates are rebuilt with `build-wasm.sh`, not `cargo build -p` here).
fn push_native_package(pkgs: &mut Vec<String>, guest: &DiscoveredGuest) {
    if guest.runtime == "workerd" {
        return;
    }
    if let Some(pkg) = &guest.package {
        push_unique(pkgs, pkg.clone());
    }
}

/// Which plugin sets `cargo build-app` / `cargo stage-plugins` should include.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `release` - When true, install under `target/release/`; otherwise `target/debug/`.
/// * `sel` - `sel` input for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn build_selection(root: &Path, release: bool, sel: BuildSelection) -> Result<()> {
    let pkgs = packages_for(root, sel)?;
    if pkgs.is_empty() {
        bail!("build selection resolved to no packages");
    }
    build_packages(root, release, &pkgs)
}

/// Stage optional and/or example guests into `dest` (catalog layout).
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `dest` - Filesystem path (`dest`).
/// * `release` - When true, install under `target/release/`; otherwise `target/debug/`.
/// * `optional` - Stage every guest under `crates/bookclerk-plugins/optional`.
/// * `examples` - Stage every guest under `examples`.
/// * `plugin_ids` - Additional individual guests by manifest id (`--plugin`).
/// * `skip_build` - Boolean flag `skip_build`.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when nothing is selected, an id is unknown or names a
/// platform guest (those are installed by `install-platform`), or the
/// underlying I/O, parse, or build operation fails.
pub fn stage_plugins(
    root: &Path,
    dest: &Path,
    release: bool,
    optional: bool,
    examples: bool,
    plugin_ids: &[String],
    skip_build: bool,
) -> Result<()> {
    if !optional && !examples && plugin_ids.is_empty() {
        bail!("stage-plugins requires --optional, --examples, and/or --plugin <id>");
    }
    let mut guests = Vec::new();
    if optional {
        guests.extend(discover_tier(root, OPTIONAL_PLUGINS_DIR)?);
    }
    if examples {
        guests.extend(discover_examples(root)?);
    }
    for guest in guests_by_id(root, plugin_ids)? {
        if is_platform_guest(&guest) {
            bail!(
                "`{}` is a platform guest; `cargo install-platform` installs it",
                guest.id
            );
        }
        if !guests.iter().any(|g| g.id == guest.id) {
            guests.push(guest);
        }
    }
    if !skip_build {
        let pkgs = packages_for_guests(&guests);
        if !pkgs.is_empty() {
            build_packages(root, release, &pkgs)?;
        }
    }
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("clear staging dir {}", dest.display()))?;
    }
    fs::create_dir_all(dest).with_context(|| format!("create staging dir {}", dest.display()))?;

    let bin_dir = root.join("target").join(profile_dir(release));
    for guest in guests {
        stage_guest(root, &bin_dir, dest, &guest, None)?;
    }
    eprintln!("BOOKCLERK_PLUGIN_ARTIFACTS={}", dest.display());
    Ok(())
}

/// Install platform guests into `$FILES_DIR/plugins/{plugin-key-fs-id}/`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `files_dir` - Bookclerk files directory to wipe and recreate.
/// * `release` - When true, install under `target/release/`; otherwise `target/debug/`.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn install_platform(root: &Path, files_dir: &Path, release: bool) -> Result<()> {
    let bin_dir = root.join("target").join(profile_dir(release));
    let guests = discover_tier(root, PLATFORM_PLUGINS_DIR)?;
    run_platform_install(files_dir, &guests, |plugins_root| {
        for guest in &guests {
            let leaf = guest_install_leaf(guest)?;
            let out = plugins_root.join(&leaf);
            if out.exists() {
                fs::remove_dir_all(&out).with_context(|| format!("clear {}", out.display()))?;
            }
            stage_guest(root, &bin_dir, plugins_root, guest, Some(files_dir))?;
            eprintln!(
                "installed platform plugin `{}` ({}) -> {}",
                guest.id,
                leaf,
                out.display()
            );
        }
        Ok(())
    })
}

/// Host-local platform install transaction: lock, preflight, then mutate.
///
/// Acquires [`PluginMutationLock`] for this `$FILES_DIR` only (not a shared
/// database lock). Occupancy is inspected before any canonical `pk-*` tree or
/// ledger row is written. An alias is a host-local presentation handle;
/// PluginKey/provenance is the durable ownership identity.
///
/// # Arguments
///
/// * `files_dir` - Host `$FILES_DIR` plugin namespace.
/// * `guests` - Platform guests that would be staged after preflight.
/// * `after_preflight` - Mutation that runs only when occupancy is clear.
///
/// # Errors
///
/// Returns when the lock cannot be acquired, occupancy cannot be proven as
/// this platform PluginKey, or `after_preflight` fails.
fn run_platform_install<T>(
    files_dir: &Path,
    guests: &[DiscoveredGuest],
    after_preflight: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let _lock =
        PluginMutationLock::acquire(files_dir).map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let plugins_root = files_dir.join("plugins");
    fs::create_dir_all(&plugins_root)
        .with_context(|| format!("create {}", plugins_root.display()))?;
    preflight_platform_alias_occupancy(&plugins_root, guests)?;
    after_preflight(&plugins_root)
}

/// Rejects platform staging when another tree already occupies a guest alias.
///
/// `plugins/<alias>/` is not proof of Bookclerk platform ownership, even when
/// `plugin.toml` says `id = "<alias>"`. Manifest alias, runtime, command, and
/// entrypoint shape are plugin-controlled and must not authorize delete or
/// overwrite. Unreleased hosts should `cargo reset --yes` (or move the tree).
///
/// Occupancy at the canonical `pk-*` leaf for this guest is the in-place
/// update path and is allowed.
///
/// # Arguments
///
/// * `plugins_root` - Host `$FILES_DIR/plugins` directory.
/// * `guests` - Platform guests about to be staged.
///
/// # Errors
///
/// Returns when a non-canonical directory occupies a platform alias, or when
/// a legacy alias path exists without a parseable *different* alias (so
/// ownership cannot be established).
fn preflight_platform_alias_occupancy(
    plugins_root: &Path,
    guests: &[DiscoveredGuest],
) -> Result<()> {
    for guest in guests {
        let canonical = plugins_root.join(guest_install_leaf(guest)?);
        let key = guest_plugin_key(guest)?;
        if let Some(path) = conflicting_alias_occupant(plugins_root, &guest.id, &canonical)? {
            bail!(
                "refusing to install platform plugin `{}` ({key}) because alias `{}` is already occupied at {}; \
                 an alias is a host-local presentation handle, not provenance, so Bookclerk will not delete \
                 or overwrite this tree. Repair this host files dir (`cargo reset --yes` or move the occupant) \
                 and retry.",
                guest.id,
                guest.id,
                path.display()
            );
        }
    }
    Ok(())
}

/// First non-canonical occupant of `alias` under `plugins_root`, if any.
///
/// # Arguments
///
/// * `plugins_root` - Host `$FILES_DIR/plugins` directory.
/// * `alias` - Manifest / display id (`sqlite`, `local`, …).
/// * `canonical` - Current `pk-*` install directory for this guest.
///
/// # Errors
///
/// Returns when `plugins_root` cannot be read or a `plugin.toml` cannot be read.
fn conflicting_alias_occupant(
    plugins_root: &Path,
    alias: &str,
    canonical: &Path,
) -> Result<Option<PathBuf>> {
    if !plugins_root.is_dir() {
        return Ok(None);
    }
    let mut unknown_legacy: Option<PathBuf> = None;
    for entry in
        fs::read_dir(plugins_root).with_context(|| format!("read {}", plugins_root.display()))?
    {
        let path = entry
            .with_context(|| format!("read entry in {}", plugins_root.display()))?
            .path();
        if !path.is_dir() {
            continue;
        }
        if paths_same_dir(&path, canonical) {
            continue;
        }
        match parsed_manifest_alias(&path)? {
            Some(id) if id.eq_ignore_ascii_case(alias) => return Ok(Some(path)),
            Some(_) => {}
            None => {
                let is_legacy_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(alias));
                if is_legacy_name && unknown_legacy.is_none() {
                    unknown_legacy = Some(path);
                }
            }
        }
    }
    Ok(unknown_legacy)
}

/// Parsed `plugin.toml` id for an install directory, if the file parses.
///
/// # Arguments
///
/// * `plugin_root` - Candidate install directory.
///
/// # Errors
///
/// Returns when `plugin.toml` exists but cannot be read.
fn parsed_manifest_alias(plugin_root: &Path) -> Result<Option<String>> {
    let toml = plugin_root.join("plugin.toml");
    if !toml.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&toml).with_context(|| format!("read {}", toml.display()))?;
    Ok(bookclerk_plugin_manifest::PluginManifest::parse(&text)
        .ok()
        .map(|manifest| manifest.id))
}

/// Returns true when `a` and `b` name the same directory after canonicalize.
///
/// Falls back to path equality when either side cannot be canonicalized.
///
/// # Arguments
///
/// * `a` - First filesystem path.
/// * `b` - Second filesystem path.
fn paths_same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Stage platform guests into a temp dir for `package-platform` bundling.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `dest` - Filesystem path (`dest`).
/// * `release` - When true, install under `target/release/`; otherwise `target/debug/`.
/// * `skip_build` - Boolean flag `skip_build`.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn stage_platform_for_pack(
    root: &Path,
    dest: &Path,
    release: bool,
    skip_build: bool,
) -> Result<()> {
    if !skip_build {
        build_selection(
            root,
            release,
            BuildSelection {
                platform: true,
                ..Default::default()
            },
        )?;
    }
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("clear staging dir {}", dest.display()))?;
    }
    fs::create_dir_all(dest).with_context(|| format!("create staging dir {}", dest.display()))?;
    let bin_dir = root.join("target").join(profile_dir(release));
    for guest in discover_tier(root, PLATFORM_PLUGINS_DIR)? {
        stage_guest(root, &bin_dir, dest, &guest, None)?;
    }
    Ok(())
}

/// Stage optional guests for `package-plugins`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `dest` - Filesystem path (`dest`).
/// * `release` - When true, install under `target/release/`; otherwise `target/debug/`.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn stage_optional_for_pack(root: &Path, dest: &Path, release: bool) -> Result<()> {
    stage_plugins(root, dest, release, true, false, &[], false)
}

/// Lists platform plugin guests under `crates/bookclerk-plugins/platform`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
///
/// # Returns
///
/// On success, the inner `Vec<DiscoveredGuest>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn discover_platform(root: &Path) -> Result<Vec<DiscoveredGuest>> {
    discover_tier(root, PLATFORM_PLUGINS_DIR)
}

/// Lists optional storefront / integration guests under `…/optional`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
///
/// # Returns
///
/// On success, the inner `Vec<DiscoveredGuest>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn discover_optional(root: &Path) -> Result<Vec<DiscoveredGuest>> {
    discover_tier(root, OPTIONAL_PLUGINS_DIR)
}

/// Lists reference Echo example guests under `examples/`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
///
/// # Returns
///
/// On success, the inner `Vec<DiscoveredGuest>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn discover_examples(root: &Path) -> Result<Vec<DiscoveredGuest>> {
    let examples = root.join(EXAMPLES_DIR);
    if !examples.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&examples).with_context(|| format!("read {}", examples.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("plugins-") {
            continue;
        }
        let dir = entry.path();
        if let Some(guest) = try_discover_guest(root, &dir)? {
            out.push(guest);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Discovers guests that have a `plugin.toml` under a workspace-relative tier directory.
fn discover_tier(root: &Path, rel: &str) -> Result<Vec<DiscoveredGuest>> {
    let base = root.join(rel);
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&base).with_context(|| format!("read {}", base.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(guest) = try_discover_guest(root, &entry.path())? {
            out.push(guest);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Reads `plugin.toml` (and optional `Cargo.toml`) from `dir`; `None` when no manifest exists.
fn try_discover_guest(root: &Path, dir: &Path) -> Result<Option<DiscoveredGuest>> {
    let manifest = dir.join("plugin.toml");
    if !manifest.is_file() {
        return Ok(None);
    }
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("read {}", manifest.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", manifest.display()))?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("{} missing id", manifest.display()))?
        .to_string();
    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("integration")
        .to_string();
    let runtime = value
        .get("runtime")
        .and_then(|v| v.as_str())
        .unwrap_or("native")
        .to_string();

    let (package, bin_name) = match read_cargo_package(dir)? {
        Some(pkg) => {
            let bin = read_cargo_bin_name(dir, &pkg)?.unwrap_or_else(|| pkg.clone());
            (Some(pkg), Some(bin))
        }
        None => (None, None),
    };

    let rel_dir = dir
        .strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");

    Ok(Some(DiscoveredGuest {
        id,
        kind,
        dir: dir.to_path_buf(),
        rel_dir,
        package,
        bin_name,
        manifest_name: "plugin.toml".into(),
        runtime,
    }))
}

/// `[package].name` from `dir/Cargo.toml`, or `None` when the file is missing.
fn read_cargo_package(dir: &Path) -> Result<Option<String>> {
    let path = dir.join("Cargo.toml");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

/// First `[[bin]].name` in `Cargo.toml`, falling back to the package name.
fn read_cargo_bin_name(dir: &Path, package: &str) -> Result<Option<String>> {
    let path = dir.join("Cargo.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if let Some(bins) = value.get("bin").and_then(|v| v.as_array()) {
        for bin in bins {
            if let Some(name) = bin.get("name").and_then(|v| v.as_str()) {
                return Ok(Some(name.to_string()));
            }
        }
    }
    Ok(Some(package.to_string()))
}

/// Cargo package names of workspace `default-members` (hosts and helper binaries).
fn default_members(root: &Path) -> Result<Vec<String>> {
    let path = root.join("Cargo.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let members = value
        .get("workspace")
        .and_then(|w| w.get("default-members"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("workspace.default-members missing"))?;
    let mut pkgs = Vec::new();
    for entry in members {
        let rel = entry
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("default-members entry is not a string"))?;
        let member_dir = root.join(rel);
        if let Some(pkg) = read_cargo_package(&member_dir)? {
            pkgs.push(pkg);
        } else {
            bail!("default-member {rel} has no [package].name");
        }
    }
    Ok(pkgs)
}

/// Copies a guest's `plugin.toml`, binary or modules, and embedded logo into the staging root.
fn stage_guest(
    _root: &Path,
    bin_dir: &Path,
    dest_root: &Path,
    guest: &DiscoveredGuest,
    files_dir: Option<&Path>,
) -> Result<()> {
    let leaf = guest_install_leaf(guest)?;
    let out = dest_root.join(leaf);
    fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;

    let manifest_src = guest.dir.join(&guest.manifest_name);
    let manifest_dest = out.join("plugin.toml");
    fs::copy(&manifest_src, &manifest_dest).with_context(|| {
        format!(
            "copy {} -> {}",
            manifest_src.display(),
            manifest_dest.display()
        )
    })?;

    // Workerd guests ship modules/ (JS / Python / Wasm glue). A companion
    // Cargo.toml may exist for Wasm crates — do not treat those as native bins.
    if guest.runtime == "workerd" {
        let modules_src = guest.dir.join("modules");
        if modules_src.is_dir() {
            copy_dir_all(&modules_src, &out.join("modules"))?;
        }
        let readme = guest.dir.join("README.md");
        if readme.is_file() {
            let _ = fs::copy(&readme, out.join("README.md"));
        }
    } else if let Some(bin_name) = &guest.bin_name {
        let src_bin = resolve_binary(bin_dir, bin_name)?;
        let dest_bin = out.join(src_bin.file_name().context("binary has no file name")?);
        fs::copy(&src_bin, &dest_bin)
            .with_context(|| format!("copy {} -> {}", src_bin.display(), dest_bin.display()))?;
        set_executable(&dest_bin)?;
        patch_command(&manifest_dest, bin_name)?;
    } else {
        bail!(
            "native guest `{}` has no Cargo package and no known script stage path ({})",
            guest.id,
            guest.dir.display()
        );
    }

    stage_embedded_logo(&guest.dir, &out, &manifest_src)?;
    stamp_platform_if_known(guest, &out, files_dir)?;
    Ok(())
}

/// Host-stamps a verified platform receipt + ledger row for installer-shipped sqlite/local.
fn stamp_platform_if_known(
    guest: &DiscoveredGuest,
    out: &Path,
    files_dir: Option<&Path>,
) -> Result<()> {
    let Some(files_dir) = files_dir else {
        return Ok(());
    };
    let Some(package) = guest.package.as_deref() else {
        return Ok(());
    };
    if bookclerk_plugin_catalog::platform_artifact(package, &guest.id).is_none() {
        return Ok(());
    }
    let text = fs::read_to_string(out.join("plugin.toml"))
        .with_context(|| format!("read {}", out.join("plugin.toml").display()))?;
    let manifest = bookclerk_plugin_manifest::PluginManifest::parse(&text)
        .with_context(|| format!("parse staged plugin.toml for {}", guest.id))?;
    let version = env!("CARGO_PKG_VERSION");
    bookclerk_plugin_catalog::stamp_platform_receipt(out, files_dir, package, &manifest, version)
        .with_context(|| format!("stamp platform receipt for {}", guest.id))?;
    Ok(())
}

/// Copy `plugin.toml` embedded `logo` path into the staged install root when present.
fn stage_embedded_logo(guest_dir: &Path, out: &Path, manifest_src: &Path) -> Result<()> {
    let text = fs::read_to_string(manifest_src)
        .with_context(|| format!("read {}", manifest_src.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", manifest_src.display()))?;
    let Some(logo) = value.get("logo").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let kind = bookclerk_plugin_manifest::validate_logo(logo)
        .with_context(|| format!("validate logo in {}", manifest_src.display()))?;
    let bookclerk_plugin_manifest::LogoKind::EmbeddedPath(rel) = kind else {
        return Ok(());
    };
    let src = guest_dir.join(&rel);
    if !src.is_file() {
        bail!(
            "embedded logo missing for staging: {} (from {})",
            src.display(),
            manifest_src.display()
        );
    }
    let dest = out.join(&rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(&src, &dest)
        .with_context(|| format!("copy logo {} -> {}", src.display(), dest.display()))?;
    Ok(())
}

/// Locates a built guest binary under `target/{debug,release}/`, including a Windows `.exe`.
fn resolve_binary(bin_dir: &Path, name: &str) -> Result<PathBuf> {
    let candidates = if cfg!(windows) {
        vec![bin_dir.join(format!("{name}.exe")), bin_dir.join(name)]
    } else {
        vec![bin_dir.join(name)]
    };
    for path in &candidates {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    bail!(
        "missing plugin binary `{name}` under {} (build the guest first)",
        bin_dir.display()
    )
}

/// Rewrites the staged `plugin.toml` `command` to `./{bin_name}` beside the manifest.
fn patch_command(manifest_path: &Path, bin_name: &str) -> Result<()> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("command") {
            lines.push(format!("command = \"./{bin_name}\""));
        } else {
            lines.push(line.to_string());
        }
    }
    let patched = if text.ends_with('\n') || lines.is_empty() {
        format!("{}\n", lines.join("\n"))
    } else {
        lines.join("\n")
    };
    fs::write(manifest_path, patched)
        .with_context(|| format!("write {}", manifest_path.display()))?;
    Ok(())
}

/// Recursively copies `src` into `dest`, creating missing directories.
fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
        let path = entry.path();
        let rel = path.strip_prefix(src).context("strip copy prefix")?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::copy(path, &out)
                .with_context(|| format!("copy {} -> {}", path.display(), out.display()))?;
        }
    }
    Ok(())
}

/// Runs `cargo build [-p …]` for the selected packages; inherits stdio and fails on non-zero exit.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `release` - Build with `--release` when true.
/// * `packages` - Cargo package names passed as `-p`.
///
/// # Errors
///
/// Returns an error when `cargo` cannot be spawned or exits non-zero.
pub fn build_packages(root: &Path, release: bool, packages: &[String]) -> Result<()> {
    let mut cmd = cargo(root);
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    for pkg in packages {
        cmd.args(["-p", pkg]);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().context("cargo build")?;
    if !status.success() {
        bail!("cargo build exited with {status}");
    }
    eprintln!(
        "built {} package(s) ({})",
        packages.len(),
        profile_dir(release)
    );
    Ok(())
}

/// `cargo` command (`$CARGO` or `cargo`) with cwd set to the workspace root.
fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root);
    cmd
}

/// Cargo profile directory name: `release` or `debug`.
fn profile_dir(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

/// Appends `pkg` when it is not already in the Cargo `-p` list.
fn push_unique(pkgs: &mut Vec<String>, pkg: String) {
    if !pkgs.iter().any(|p| p == &pkg) {
        pkgs.push(pkg);
    }
}

#[cfg(unix)]
/// Sets Unix mode `0o755` on a staged launcher or binary.
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("metadata {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn platform_guest(id: &str) -> DiscoveredGuest {
        discover_platform(&workspace_root())
            .expect("platform guests")
            .into_iter()
            .find(|g| g.id == id)
            .unwrap_or_else(|| panic!("missing platform guest {id}"))
    }

    fn write_plugin_toml(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("plugin.toml"), body).unwrap();
    }

    fn sqlite_alias_toml() -> &'static str {
        r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./bin"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"
"#
    }

    fn snapshot_tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry.unwrap();
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(dir).unwrap();
            files.push((
                rel.to_string_lossy().replace('\\', "/"),
                fs::read(entry.path()).unwrap(),
            ));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    fn pk_leaves(plugins: &Path) -> Vec<String> {
        let mut leaves = Vec::new();
        if !plugins.is_dir() {
            return leaves;
        }
        for entry in fs::read_dir(plugins).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            if name.starts_with("pk-") {
                leaves.push(name.into_owned());
            }
        }
        leaves.sort();
        leaves
    }

    #[test]
    fn foreign_legacy_sqlite_alias_blocks_platform_install_before_mutation() {
        let files = tempfile::tempdir().unwrap();
        let plugins = files.path().join("plugins");
        let legacy = plugins.join("sqlite");
        write_plugin_toml(&legacy, sqlite_alias_toml());
        fs::write(legacy.join("keep-me.txt"), b"third-party").unwrap();
        let unrelated = plugins.join("echo");
        write_plugin_toml(
            &unrelated,
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./bin"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#,
        );
        let state = files.path().join("plugin-state").join("unrelated");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("data"), b"stay").unwrap();
        let before_plugins = snapshot_tree(&plugins);
        let before_state = snapshot_tree(&files.path().join("plugin-state"));
        let guests = vec![platform_guest("sqlite"), platform_guest("local")];
        let mut mutated = false;
        let err = run_platform_install(files.path(), &guests, |_| {
            mutated = true;
            Ok(())
        })
        .expect_err("unproven alias occupancy must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("cargo reset --yes"),
            "actionable repair: {msg}"
        );
        assert!(
            msg.contains("presentation handle"),
            "alias is not provenance: {msg}"
        );
        assert!(!mutated, "must fail before staging or ledger writes");
        assert_eq!(snapshot_tree(&plugins), before_plugins);
        assert_eq!(
            snapshot_tree(&files.path().join("plugin-state")),
            before_state
        );
        assert!(legacy.join("keep-me.txt").is_file());
        assert!(unrelated.join("plugin.toml").is_file());
        assert_eq!(fs::read(state.join("data")).unwrap(), b"stay");
        assert!(pk_leaves(&plugins).is_empty(), "no canonical pk-* commit");
        assert!(
            bookclerk_plugin_catalog::InstallLedger::load(files.path())
                .unwrap()
                .artifacts
                .is_empty(),
            "no platform ledger row"
        );
    }

    #[test]
    fn manifest_alias_runtime_and_entrypoints_are_not_provenance() {
        let files = tempfile::tempdir().unwrap();
        let plugins = files.path().join("plugins");
        let legacy = plugins.join("sqlite");
        write_plugin_toml(&legacy, sqlite_alias_toml());
        fs::write(
            legacy.join("receipt.json"),
            br#"{
  "schema_version": 2,
  "plugin_key": "platform:bookclerk/bookclerk-plugin-database-sqlite",
  "provenance": "platform_bundled",
  "coordinate": {"source": "localArchive", "name": "bookclerk/bookclerk-plugin-database-sqlite", "version": "0.1.0"},
  "version": "0.1.0",
  "artifact_url": "platform:bookclerk/bookclerk-plugin-database-sqlite",
  "target": "linux-x64-gnu",
  "archive_sha256": "",
  "manifest_sha256": "aa",
  "payload_root_sha256": "bb",
  "protocol": "workers-rpc",
  "api_version": 3,
  "runtime": {"kind": "database", "id": "sqlite"},
  "requested_sandbox": {"network": "deny"},
  "approved_network": "deny",
  "installed_at": "2020-01-01T00:00:00Z"
}"#,
        )
        .unwrap();
        let guests = vec![platform_guest("sqlite")];
        let mut mutated = false;
        let err = run_platform_install(files.path(), &guests, |_| {
            mutated = true;
            Ok(())
        })
        .expect_err("plugin-controlled receipt/manifest must not authorize install");
        assert!(err.to_string().contains("sqlite"));
        assert!(!mutated);
        assert!(legacy.join("plugin.toml").is_file());
        assert!(legacy.join("receipt.json").is_file());
        assert!(pk_leaves(&plugins).is_empty());
        assert!(bookclerk_plugin_catalog::InstallLedger::load(files.path())
            .unwrap()
            .artifacts
            .is_empty());
    }

    #[test]
    fn canonical_platform_install_update_preflight_still_succeeds() {
        let files = tempfile::tempdir().unwrap();
        let guest = platform_guest("sqlite");
        let plugins = files.path().join("plugins");
        let dest = plugins.join(guest_install_leaf(&guest).unwrap());
        write_plugin_toml(&dest, sqlite_alias_toml());
        fs::write(dest.join("bin"), b"guest").unwrap();
        let text = fs::read_to_string(dest.join("plugin.toml")).unwrap();
        let manifest = bookclerk_plugin_manifest::PluginManifest::parse(&text).unwrap();
        bookclerk_plugin_catalog::stamp_platform_receipt(
            &dest,
            files.path(),
            guest.package.as_deref().expect("sqlite package"),
            &manifest,
            "0.1.0",
        )
        .unwrap();
        let guests = vec![guest];
        let mut ran = false;
        run_platform_install(files.path(), &guests, |_| {
            ran = true;
            Ok(())
        })
        .expect("canonical PluginKey path is the update target");
        assert!(ran);
        assert!(dest.join("plugin.toml").is_file());
        assert!(bookclerk_plugin_catalog::InstallLedger::load(files.path())
            .unwrap()
            .get(&guest_plugin_key(&guests[0]).unwrap())
            .is_some());
    }

    #[test]
    fn distinct_files_dirs_do_not_share_alias_occupancy() {
        let occupied = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        write_plugin_toml(
            &occupied.path().join("plugins").join("sqlite"),
            sqlite_alias_toml(),
        );
        let guests = vec![platform_guest("sqlite")];
        run_platform_install(occupied.path(), &guests, |_| -> Result<()> {
            panic!("occupied host must not mutate")
        })
        .expect_err("occupied host");
        let mut ran = false;
        run_platform_install(other.path(), &guests, |_| {
            ran = true;
            Ok(())
        })
        .expect("separate $FILES_DIR is an independent plugin namespace");
        assert!(ran);
        assert!(occupied
            .path()
            .join("plugins")
            .join("sqlite")
            .join("plugin.toml")
            .is_file());
        assert!(pk_leaves(&other.path().join("plugins")).is_empty());
    }

    #[test]
    fn legacy_alias_dir_with_different_manifest_id_is_not_deleted() {
        let files = tempfile::tempdir().unwrap();
        let plugins = files.path().join("plugins");
        let named = plugins.join("sqlite");
        write_plugin_toml(
            &named,
            r#"
api_version = 3
id = "other"
runtime = "native"
command = "./bin"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#,
        );
        let guests = vec![platform_guest("sqlite")];
        let mut ran = false;
        run_platform_install(files.path(), &guests, |_| {
            ran = true;
            Ok(())
        })
        .expect("different alias occupancy is not a sqlite collision");
        assert!(ran);
        assert!(named.join("plugin.toml").is_file(), "must not delete");
    }

    #[test]
    fn discovers_platform_sqlite_and_local() {
        let root = workspace_root();
        let guests = discover_platform(&root).expect("platform");
        let ids: HashSet<_> = guests.iter().map(|g| g.id.as_str()).collect();
        assert!(ids.contains("sqlite"), "{ids:?}");
        assert!(ids.contains("local"), "{ids:?}");
        let sqlite = guests.iter().find(|g| g.id == "sqlite").unwrap();
        assert_eq!(
            sqlite.package.as_deref(),
            Some("bookclerk-plugin-database-sqlite")
        );
    }

    #[test]
    fn discovers_optional_audible_and_d1() {
        let root = workspace_root();
        let guests = discover_optional(&root).expect("optional");
        let ids: HashSet<_> = guests.iter().map(|g| g.id.as_str()).collect();
        assert!(ids.contains("audible"), "{ids:?}");
        assert!(ids.contains("d1"), "{ids:?}");
        assert!(ids.contains("postgres"), "{ids:?}");
        assert!(!ids.contains("sqlite"));
    }

    #[test]
    fn packages_for_platform_includes_default_members() {
        let root = workspace_root();
        let pkgs = packages_for(
            &root,
            BuildSelection {
                platform: true,
                ..Default::default()
            },
        )
        .expect("packages");
        assert!(pkgs.iter().any(|p| p == "bookclerkd"));
        assert!(pkgs.iter().any(|p| p == "bookclerk-cli"));
        assert!(pkgs.iter().any(|p| p == "bookclerk-jail"));
        assert!(pkgs.iter().any(|p| p == "bookclerk-workerd"));
        assert!(pkgs.iter().any(|p| p == "bookclerk-media-worker"));
        assert!(pkgs.iter().any(|p| p == "bookclerk-plugin-database-sqlite"));
        assert!(!pkgs.iter().any(|p| p.contains("audible")));
    }

    #[test]
    fn packages_for_platform_excludes_optional_guests() {
        let root = workspace_root();
        let pkgs = packages_for(
            &root,
            BuildSelection {
                platform: true,
                ..Default::default()
            },
        )
        .expect("packages");
        assert!(pkgs.iter().any(|p| p == "bookclerk-cli"));
        assert!(pkgs.iter().any(|p| p == "bookclerkd"));
        assert!(!pkgs.iter().any(|p| p.contains("audible")));
        assert!(!pkgs.iter().any(|p| p.contains("postgres")));
        assert!(!pkgs.iter().any(|p| p.contains("echo")));
        let mut seen = HashSet::new();
        for pkg in &pkgs {
            assert!(
                seen.insert(pkg.as_str()),
                "duplicate package in list: {pkg}"
            );
        }
    }

    #[test]
    fn packages_for_examples_skips_workerd_wasm_crate() {
        let root = workspace_root();
        let pkgs = packages_for(
            &root,
            BuildSelection {
                examples: true,
                ..Default::default()
            },
        )
        .expect("packages");
        assert!(pkgs
            .iter()
            .any(|p| p == "bookclerk-plugin-echo-native-rust"));
        assert!(
            !pkgs
                .iter()
                .any(|p| p == "bookclerk-plugin-echo-workerd-rust"),
            "workerd Wasm crate must not be built as a native package: {pkgs:?}"
        );
    }

    #[test]
    fn example_ids_are_distinct() {
        let root = workspace_root();
        let guests = discover_examples(&root).expect("examples");
        let mut seen = HashSet::new();
        for g in &guests {
            assert!(seen.insert(g.id.as_str()), "duplicate {}", g.id);
        }
        assert!(seen.contains("echo_native_rust"));
        assert!(seen.contains("echo_native_node"));
        assert!(seen.contains("echo_native_python"));
        assert!(seen.contains("echo_workerd_ts"));
        assert!(seen.contains("echo_workerd_python"));
        assert!(seen.contains("echo_workerd_rust"));
        assert!(seen.contains("echo_workerd_fetch"));
    }

    #[test]
    fn guests_by_id_resolves_across_tiers_in_request_order() {
        let root = workspace_root();
        let ids = ["libro", "echo_workerd_ts", "sqlite", "libro"].map(String::from);
        let guests = guests_by_id(&root, &ids).expect("resolve");
        let got: Vec<_> = guests.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(got, ["libro", "echo_workerd_ts", "sqlite"]);
        assert_eq!(
            packages_for_guests(&guests),
            [
                "bookclerk-plugin-source-libro",
                "bookclerk-plugin-database-sqlite"
            ],
            "workerd guests contribute no Cargo package"
        );
    }

    #[test]
    fn guests_by_id_rejects_unknown_ids() {
        let root = workspace_root();
        let err = guests_by_id(&root, &["libro".into(), "nope".into()]).unwrap_err();
        assert!(err.to_string().contains("\"nope\""), "{err}");
    }

    #[test]
    fn stage_plugins_rejects_platform_ids_and_empty_selection() {
        let root = workspace_root();
        let dest = tempfile::tempdir().expect("tempdir");
        let err = stage_plugins(
            &root,
            dest.path(),
            false,
            false,
            false,
            &["sqlite".into()],
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("platform guest"), "{err}");
        let err = stage_plugins(&root, dest.path(), false, false, false, &[], true).unwrap_err();
        assert!(err.to_string().contains("--plugin"), "{err}");
    }
}
