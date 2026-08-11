//! Directory-driven discovery, build, stage, and platform install for guests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const PLATFORM_PLUGINS_DIR: &str = "crates/bookclerk-plugins/platform";
const OPTIONAL_PLUGINS_DIR: &str = "crates/bookclerk-plugins/optional";
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
    /// Workspace `default-members` + platform guests under [`PLATFORM_PLUGINS_DIR`].
    pub platform: bool,
    /// Guests under [`OPTIONAL_PLUGINS_DIR`].
    pub optional: bool,
    /// Guests under [`EXAMPLES_DIR`].
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
/// * `optional` - Boolean flag `optional`.
/// * `examples` - Boolean flag `examples`.
/// * `skip_build` - Boolean flag `skip_build`.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn stage_plugins(
    root: &Path,
    dest: &Path,
    release: bool,
    optional: bool,
    examples: bool,
    skip_build: bool,
) -> Result<()> {
    if !optional && !examples {
        bail!("stage-plugins requires --optional and/or --examples");
    }
    if !skip_build {
        build_selection(
            root,
            release,
            BuildSelection {
                optional,
                examples,
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
    let mut guests = Vec::new();
    if optional {
        guests.extend(discover_tier(root, OPTIONAL_PLUGINS_DIR)?);
    }
    if examples {
        guests.extend(discover_examples(root)?);
    }
    for guest in guests {
        stage_guest(root, &bin_dir, dest, &guest)?;
    }
    eprintln!("BOOKCLERK_PLUGIN_ARTIFACTS={}", dest.display());
    Ok(())
}

/// Install platform guests into `$FILES_DIR/plugins/{id}/` (installer layout).
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
    let plugins_root = files_dir.join("plugins");
    fs::create_dir_all(&plugins_root)
        .with_context(|| format!("create {}", plugins_root.display()))?;
    let bin_dir = root.join("target").join(profile_dir(release));
    for guest in discover_tier(root, PLATFORM_PLUGINS_DIR)? {
        let out = plugins_root.join(&guest.id);
        if out.exists() {
            fs::remove_dir_all(&out).with_context(|| format!("clear {}", out.display()))?;
        }
        stage_guest(root, &bin_dir, &plugins_root, &guest)?;
        eprintln!(
            "installed platform plugin `{}` -> {}",
            guest.id,
            out.display()
        );
    }
    Ok(())
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
        stage_guest(root, &bin_dir, dest, &guest)?;
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
    stage_plugins(root, dest, release, true, false, false)
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

fn stage_guest(
    root: &Path,
    bin_dir: &Path,
    dest_root: &Path,
    guest: &DiscoveredGuest,
) -> Result<()> {
    let out = dest_root.join(&guest.id);
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
    } else if guest.id == "echo_native_node" {
        stage_echo_native_node(root, &guest.dir, &out, &manifest_dest)?;
    } else if guest.id == "echo_native_python" {
        stage_echo_native_python(root, &guest.dir, &out, &manifest_dest)?;
    } else {
        bail!(
            "native guest `{}` has no Cargo package and no known script stage path ({})",
            guest.id,
            guest.dir.display()
        );
    }

    stage_embedded_logo(&guest.dir, &out, &manifest_src)?;
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

/// Stage Node Echo as an executable launcher + vendored `@bookclerk/plugin-sdk/native`.
///
/// SEA packaging remains the publisher path (`scripts/build-sea.mjs`); CI/dev use
/// this wrapper so handshake smoke works without postject.
///
/// The Node interpreter is vendored under `runtime/` (hardlink when possible) so
/// the guest jail can exec it — host PATH entries like GitHub Actions'
/// `/opt/hostedtoolcache/...` are outside Landlock `system_paths`.
fn stage_echo_native_node(
    root: &Path,
    guest_dir: &Path,
    out: &Path,
    manifest_dest: &Path,
) -> Result<()> {
    let sdk_dist = root.join("packages/plugin-sdk/dist");
    let native_js = sdk_dist.join("native.js");
    if !native_js.is_file() {
        bail!(
            "missing {} — run `npm run build` in packages/plugin-sdk before staging echo_native_node",
            native_js.display()
        );
    }
    let sdk_out = out.join("sdk");
    fs::create_dir_all(&sdk_out)?;
    for name in ["native.js", "generated.js"] {
        let src = sdk_dist.join(name);
        if !src.is_file() {
            bail!("missing SDK dist file {}", src.display());
        }
        fs::copy(&src, sdk_out.join(name))?;
    }
    let script_src = guest_dir.join("src/echo.mjs");
    if !script_src.is_file() {
        bail!("missing {}", script_src.display());
    }
    fs::create_dir_all(out.join("src"))?;
    fs::copy(&script_src, out.join("src/echo.mjs"))?;
    let readme = guest_dir.join("README.md");
    if readme.is_file() {
        let _ = fs::copy(&readme, out.join("README.md"));
    }

    let node_src = resolve_host_command("BOOKCLERK_NODE", &["node"])?;
    let node_name = if cfg!(windows) { "node.exe" } else { "node" };
    let node_dest = out.join("runtime").join(node_name);
    vendor_into(&node_src, &node_dest).with_context(|| {
        format!(
            "vendor node {} -> {}",
            node_src.display(),
            node_dest.display()
        )
    })?;
    set_executable(&node_dest)?;

    let bin_name = "bookclerk-plugin-echo-native-node";
    let launcher = out.join(bin_name);
    let body = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nHERE=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\nexport BOOKCLERK_PLUGIN_SDK_NATIVE=\"$HERE/sdk/native.js\"\nexec \"$HERE/runtime/{node_name}\" \"$HERE/src/echo.mjs\"\n"
    );
    fs::write(&launcher, body)?;
    set_executable(&launcher)?;
    patch_command(manifest_dest, bin_name)?;
    Ok(())
}

/// Resolve `ENV` if set, otherwise the first `names` entry found on `PATH`.
fn resolve_host_command(env_key: &str, names: &[&str]) -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var(env_key) {
        let path = PathBuf::from(&explicit);
        if path.is_file() {
            return Ok(path);
        }
        bail!("{env_key}={explicit} is not a file");
    }
    let path_os = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_os) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
            #[cfg(windows)]
            {
                let with_exe = dir.join(format!("{name}.exe"));
                if with_exe.is_file() {
                    return Ok(with_exe);
                }
            }
        }
    }
    bail!(
        "could not find {} on PATH (set {env_key} to an absolute interpreter path)",
        names.join("/")
    );
}

/// Prefer a hard link into the staged tree; fall back to copy (cross-device).
fn vendor_into(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if dest.exists() {
        fs::remove_file(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    if fs::hard_link(src, dest).is_ok() {
        return Ok(());
    }
    fs::copy(src, dest).map(|_| ()).with_context(|| {
        format!(
            "hardlink/copy {} -> {} failed",
            src.display(),
            dest.display()
        )
    })
}

/// Stage Python Echo as an executable launcher + vendored `bookclerk_plugin_sdk`.
fn stage_echo_native_python(
    root: &Path,
    guest_dir: &Path,
    out: &Path,
    manifest_dest: &Path,
) -> Result<()> {
    let sdk_src = root.join("packages/plugin-sdk-python/src/bookclerk_plugin_sdk");
    if !sdk_src.is_dir() {
        bail!("missing Python SDK at {}", sdk_src.display());
    }
    copy_dir_all(&sdk_src, &out.join("sdk/bookclerk_plugin_sdk"))?;
    let script_src = guest_dir.join("echo_plugin.py");
    if !script_src.is_file() {
        bail!("missing {}", script_src.display());
    }
    fs::copy(&script_src, out.join("echo_plugin.py"))?;
    let readme = guest_dir.join("README.md");
    if readme.is_file() {
        let _ = fs::copy(&readme, out.join("README.md"));
    }

    let bin_name = "bookclerk-plugin-echo-native-python";
    let launcher = out.join(bin_name);
    let body = "#!/usr/bin/env bash\nset -euo pipefail\nHERE=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\nexport BOOKCLERK_PLUGIN_SDK_PYTHON=\"$HERE/sdk\"\nexport PYTHONPATH=\"$HERE/sdk${PYTHONPATH:+:$PYTHONPATH}\"\nexec \"${BOOKCLERK_PYTHON:-python3}\" \"$HERE/echo_plugin.py\"\n";
    fs::write(&launcher, body)?;
    set_executable(&launcher)?;
    patch_command(manifest_dest, bin_name)?;
    Ok(())
}

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

fn build_packages(root: &Path, release: bool, packages: &[String]) -> Result<()> {
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

fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root);
    cmd
}

fn profile_dir(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

fn push_unique(pkgs: &mut Vec<String>, pkg: String) {
    if !pkgs.iter().any(|p| p == &pkg) {
        pkgs.push(pkg);
    }
}

#[cfg(unix)]
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
        assert!(seen.contains("echo_workerd_ts"));
        assert!(seen.contains("echo_workerd_python"));
        assert!(seen.contains("echo_workerd_rust"));
    }
}
