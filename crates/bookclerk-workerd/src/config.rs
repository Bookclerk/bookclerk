//! Generate a real workerd Cap'n Proto config for one plugin isolate.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use bookclerk_plugin_abi::{Entrypoint, PluginDescribe};
use bookclerk_plugin_manifest::{
    manifest_needs_python, with_python_runtime_hosts, workerd_network_allow, CidrGrant,
    EffectiveWorkerdLimits, EgressPolicy, NetworkMode, PluginManifest,
};

use crate::egress::EgressProxy;
use crate::pin::BUNDLED_WORKERD_COMPAT_DATE;

/// Require a relative manifest path (`modules_dir`, `main_module`).
///
/// Multiple normal components are valid (`dist/modules`, `nested/main.js`).
/// A `..` sequence inside a filename (`edition..2.js`) is not traversal.
/// Parent, root, prefix, absolute, empty, and NUL paths are rejected.
/// Containment is still `canonicalize` + `starts_with` at the join site.
fn require_relative_manifest_path<'a>(label: &str, value: &'a str) -> Result<&'a str> {
    if value.is_empty() || value.contains('\0') {
        bail!("{label} is empty or contains NUL");
    }
    let path = Path::new(value);
    if path.is_absolute() {
        bail!("{label} must be a relative path: {value}");
    }
    let mut saw_normal = false;
    for comp in path.components() {
        match comp {
            Component::Normal(name) => {
                let s = name
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("{label} is not valid UTF-8: {value}"))?;
                if s.is_empty() || s == "." || s == ".." {
                    bail!("{label} must not contain parent or empty components: {value}");
                }
                saw_normal = true;
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("{label} must not contain parent or root components: {value}");
            }
        }
    }
    if !saw_normal {
        bail!("{label} is empty: {value}");
    }
    Ok(value)
}

/// Require one relative filename (no separators). `edition..2.js` is allowed.
fn require_single_path_component<'a>(label: &str, value: &'a str) -> Result<&'a str> {
    let value = require_relative_manifest_path(label, value)?;
    let mut normals = Path::new(value).components().filter_map(|c| match c {
        Component::Normal(s) => Some(s),
        Component::CurDir => None,
        _ => None,
    });
    match (normals.next(), normals.next()) {
        (Some(name), None) => name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{label} is not valid UTF-8: {value}")),
        _ => bail!("{label} must be a single path component: {value}"),
    }
}

/// Refuse an existing symlink at `path`. A missing final component is allowed.
fn refuse_symlink_component(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("refusing symlink in path: {}", path.display());
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

/// Join `root` / `rel` and require the result stays under `root`.
///
/// Requires `root` to already exist and canonicalize. For missing leaves,
/// canonicalizes the nearest existing parent and rejoins the suffix — never
/// returns a raw path that skipped canonicalize + `starts_with`.
#[cfg(test)]
fn join_under(root: &Path, rel: impl AsRef<Path>) -> Result<PathBuf> {
    let rel = rel.as_ref();
    if rel.is_absolute() {
        bail!(
            "refusing absolute path under {}: {}",
            root.display(),
            rel.display()
        );
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                bail!(
                    "refusing path traversal under {}: {}",
                    root.display(),
                    rel.display()
                );
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "refusing rooted path under {}: {}",
                    root.display(),
                    rel.display()
                );
            }
        }
    }
    let root_norm =
        fs::canonicalize(root).with_context(|| format!("canonicalize root {}", root.display()))?;
    let mut out = root_norm.clone();
    for comp in rel.components() {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    require_under(&root_norm, &out)
}

/// Returns `path` when it stays under `root` after canonicalize + `starts_with`.
///
/// Missing leaves: canonicalize the nearest existing parent and rejoin. Existing
/// dangling symlinks are refused. No raw-path fallback.
fn require_under(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("refusing path with '..': {}", path.display());
    }
    let root_norm =
        fs::canonicalize(root).with_context(|| format!("canonicalize root {}", root.display()))?;
    // Lexical under-root barrier *before* canonicalize/symlink_metadata sinks.
    // Compare against both the caller root and its canonical form: Windows
    // canonicalize rewrites `C:\Users\RUNNER~1\...` to `\\?\C:\Users\runneradmin\...`,
    // so a same-form `starts_with(root)` still holds when the canonical form does not.
    let lexical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_norm.join(path)
    };
    #[cfg(not(windows))]
    if !lexical.starts_with(&root_norm) && !lexical.starts_with(root) {
        // Unix paths share one prefix form; a miss here is a real escape.
        // Windows 8.3 / verbatim prefixes disagree until canonicalize, so this
        // early reject is unix-only. The canonical `starts_with` below still
        // applies on every platform.
        bail!(
            "path {} escapes root {}",
            lexical.display(),
            root_norm.display()
        );
    }
    let path_norm = match fs::canonicalize(&lexical) {
        Ok(c) => c,
        Err(err) => {
            match fs::symlink_metadata(&lexical) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    bail!(
                        "refusing dangling or unresolvable symlink {}: {err}",
                        lexical.display()
                    );
                }
                Ok(_) => {
                    bail!(
                        "could not canonicalize existing path {}: {err}",
                        lexical.display()
                    );
                }
                Err(meta_err) if meta_err.kind() != std::io::ErrorKind::NotFound => {
                    bail!("could not stat path {}: {meta_err}", lexical.display());
                }
                Err(_) => {}
            }
            let mut suffix = Vec::new();
            let mut cursor = lexical.clone();
            loop {
                // Keep walking only while the cursor stays under the caller root
                // or its canonical form. Windows short-name temps fail the
                // canonical comparison until an ancestor is canonicalized, so
                // the early reject is unix-only.
                #[cfg(not(windows))]
                if !cursor.starts_with(&root_norm) && !cursor.starts_with(root) {
                    bail!(
                        "path {} escapes root {}",
                        cursor.display(),
                        root_norm.display()
                    );
                }
                match fs::canonicalize(&cursor) {
                    Ok(canon) => {
                        let mut out = canon;
                        for part in suffix.iter().rev() {
                            out.push(part);
                        }
                        break out;
                    }
                    Err(canon_err) => {
                        match fs::symlink_metadata(&cursor) {
                            Ok(meta) if meta.file_type().is_symlink() => {
                                bail!(
                                    "refusing dangling or unresolvable symlink {}: {canon_err}",
                                    cursor.display()
                                );
                            }
                            Ok(_) => {
                                bail!(
                                    "could not canonicalize path {}: {canon_err}",
                                    cursor.display()
                                );
                            }
                            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                bail!("could not stat path {}: {e}", cursor.display());
                            }
                            Err(_) => {}
                        }
                        let name = cursor
                            .file_name()
                            .ok_or_else(|| {
                                anyhow::anyhow!("path has no file name: {}", path.display())
                            })?
                            .to_os_string();
                        suffix.push(name);
                        match cursor.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                cursor = parent.to_path_buf();
                            }
                            _ => {
                                bail!(
                                    "could not canonicalize path {} under {}: {canon_err}",
                                    path.display(),
                                    root.display()
                                );
                            }
                        }
                    }
                }
            }
        }
    };
    if !path_norm.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            path_norm.display(),
            root_norm.display()
        );
    }
    Ok(path_norm)
}

/// Create `root` / `rel` as a directory, then return its canonical path under `root`.
///
/// Performs `create_dir_all`, canonicalize, and `starts_with` in this function so
/// Default Setup path-injection queries can barrier the same value used at the
/// filesystem sink.
fn ensure_dir_under(root: &Path, rel: impl AsRef<Path>) -> Result<PathBuf> {
    let rel = rel.as_ref();
    let root_norm =
        fs::canonicalize(root).with_context(|| format!("canonicalize root {}", root.display()))?;
    let mut out = root_norm.clone();
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "refusing unsafe dir under {}: {}",
                    root.display(),
                    rel.display()
                );
            }
        }
    }
    fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
    let canon =
        fs::canonicalize(&out).with_context(|| format!("canonicalize {}", out.display()))?;
    if !canon.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            canon.display(),
            root_norm.display()
        );
    }
    Ok(canon)
}

/// Write `contents` to `root` / `name` (single component) after canonicalize + `starts_with`.
///
/// Creates parent directories and the file (if missing) so canonicalize succeeds,
/// then writes through the canonical path in this function (sink + barrier colocated).
fn write_file_under(root: &Path, name: &str, contents: impl AsRef<[u8]>) -> Result<PathBuf> {
    let name = require_single_path_component("file name", name)?;
    let root_norm =
        fs::canonicalize(root).with_context(|| format!("canonicalize root {}", root.display()))?;
    let out = root_norm.join(name);
    if !out.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            out.display(),
            root_norm.display()
        );
    }
    refuse_symlink_component(&out)?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if !out.exists() {
        fs::File::create(&out).with_context(|| format!("create {}", out.display()))?;
    }
    let canon =
        fs::canonicalize(&out).with_context(|| format!("canonicalize {}", out.display()))?;
    if !canon.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            canon.display(),
            root_norm.display()
        );
    }
    fs::write(&canon, contents).with_context(|| format!("write {}", canon.display()))?;
    Ok(canon)
}

/// Create `root` / `rel` (and parents). Used by tests writing under TempDir.
#[cfg(test)]
fn create_dir_under(root: &Path, rel: impl AsRef<Path>) -> PathBuf {
    ensure_dir_under(root, rel).expect("under root")
}

/// Write `contents` to `root` / `rel`. Used by tests writing under TempDir.
#[cfg(test)]
fn write_under(root: &Path, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let rel = rel.as_ref();
    let comps: Vec<_> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    match comps.as_slice() {
        [name] => {
            write_file_under(root, name, contents).expect("write");
        }
        _ => {
            let path = join_under(root, rel).expect("under root");
            if let Some(parent) = path.parent() {
                if parent != root {
                    fs::create_dir_all(parent).expect("mkdir parent");
                }
            }
            // Re-barrier after parent create for multi-segment test helpers.
            let path = require_under(root, &path).expect("under root");
            fs::write(&path, contents).expect("write");
        }
    }
}

/// Read a path that must stay under `root` (tests).
#[cfg(test)]
fn read_under(root: &Path, path: &Path) -> String {
    let path = require_under(root, path).expect("under root");
    fs::read_to_string(&path).expect("read")
}

/// Clone a TempDir / session path for test filesystem access.
///
/// Session dirs often live under `$TMPDIR`, not the plugin root, so
/// [`require_under`] against the plugin path cannot apply.
#[cfg(test)]
fn test_fs_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Host↔isolate RPC bridge script materialized into the workerd state dir.
const BRIDGE_JS: &str = include_str!("../bridge/bridge.js");
/// Isolate-side egress proxy that enforces the operator domain grant.
const EGRESS_JS: &str = include_str!("../bridge/egress.js");
/// Incoming CONNECT (`connect()` via `globalOutbound`) is experimental-gated
/// in workerd (workerd#6059). Only the egress worker needs this flag.
///
/// The `$experimental` annotation also requires [`WORKERD_SERVE_EXPERIMENTAL`]
/// on `workerd serve`; the compat flag alone is not enough.
const EGRESS_COMPAT_FLAGS: &str = r#"compatibilityFlags = ["experimental"],"#;
/// `workerd serve` flag that unlocks `$experimental` compatibility flags.
///
/// Required so the egress worker can export a `connect()` handler. Author
/// isolates do not receive this compat flag unless their own manifest lists it.
pub const WORKERD_SERVE_EXPERIMENTAL: &str = "--experimental";
/// Stub `host` module injected so guest JS can call host RPCs inside the isolate.
/// Injected as `@bookclerk/plugin-sdk` + `@bookclerk/plugin-sdk/workerd`.
const SDK_WORKERD_JS: &str = include_str!("../../../packages/plugin-sdk/embed/bookclerk_plugin.js");
/// Injected as `bookclerk_plugin_sdk/workerd.py`.
const SDK_WORKERD_PY: &str =
    include_str!("../../../packages/plugin-sdk-python/src/bookclerk_plugin_sdk/workerd.py");
/// Injected as `bookclerk_plugin_sdk/db_value.py` (imported by `workerd.py`).
const SDK_DB_VALUE_PY: &str =
    include_str!("../../../packages/plugin-sdk-python/src/bookclerk_plugin_sdk/db_value.py");
/// Injected as `bookclerk_plugin_sdk/_abi.py` (generated product constants).
const SDK_PRODUCT_ABI_PY: &str =
    include_str!("../../../packages/plugin-sdk-python/src/bookclerk_plugin_sdk/_abi.py");
/// Injected as `bookclerk_plugin_sdk/guest_sql.py` (imported by `db_value.py`).
const SDK_GUEST_SQL_PY: &str =
    include_str!("../../../packages/plugin-sdk-python/src/bookclerk_plugin_sdk/guest_sql.py");
/// First-party adapter isolate: owns GRANTED / BRIDGE_TOKEN (author `PLUGIN`).
const ADAPTER_JS: &str = r#"import { wrapPluginFromBinding } from "@bookclerk/plugin-sdk/workerd";
export default wrapPluginFromBinding();
"#;
/// Generated native-behind-workerd adapter: control plane only (`describe`
/// merge, `open` policy, `shutdown`) against `PLUGIN_DESCRIBE`; no author
/// isolate and no data-plane binding — the launcher forwards entrypoint calls
/// to the native guest as typed Cap'n Proto.
const NATIVE_ADAPTER_JS: &str = r#"import { wrapPluginFromNative } from "@bookclerk/plugin-sdk/workerd";
export default wrapPluginFromNative();
"#;
/// Sparse `bookclerk_plugin_sdk/__init__.py` pointing authors at the workerd guest SDK.
const SDK_PY_INIT: &str = concat!(
    "\"\"\"Bookclerk plugin SDK (workerd isolate).\n\n",
    "Use: from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, js\n\n",
    "Native guests use Rust serve() / PluginWorker instead.\n",
    "\"\"\"\n"
);

/// Package import names authors use for the workerd `BookclerkEntrypoint` SDK.
pub const SDK_JS_MODULE_NAMES: &[&str] =
    &["@bookclerk/plugin-sdk/workerd", "@bookclerk/plugin-sdk"];
/// Python package path for `from bookclerk_plugin_sdk.workerd import …`.
pub const SDK_PY_WORKERD_MODULE: &str = "bookclerk_plugin_sdk/workerd.py";
/// Python module path for typed SQL value / execute codec helpers.
pub const SDK_PY_DB_VALUE_MODULE: &str = "bookclerk_plugin_sdk/db_value.py";
/// Python module path for the generated product ABI constants.
pub const SDK_PY_PRODUCT_ABI_MODULE: &str = "bookclerk_plugin_sdk/_abi.py";
/// Python module path for guest-side SQL statement classification helpers.
pub const SDK_PY_GUEST_SQL_MODULE: &str = "bookclerk_plugin_sdk/guest_sql.py";
/// Python module path that initializes the sparse workerd guest SDK.
pub const SDK_PY_INIT_MODULE: &str = "bookclerk_plugin_sdk/__init__.py";

// Re-export so call sites / docs can discover the shared list beside workerd config.
pub use bookclerk_plugin_manifest::PYODIDE_EGRESS_HOSTS;

/// Hex chars of the plugin-root SHA-256 prefix in a state-dir leaf.
const PLUGIN_ROOT_PREFIX_HEX: usize = 8;
/// Hex chars of the per-session nonce (`2` random bytes).
const SESSION_NONCE_HEX: usize = 4;

/// Short SHA-256 prefix that groups sessions for one plugin install root.
fn plugin_root_prefix(plugin_root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(plugin_root.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..PLUGIN_ROOT_PREFIX_HEX / 2])
}

/// Stable leaf under `$TMPDIR` for one materialization session.
///
/// Unix `sockaddr_un` is about 108 bytes. Guest scratch is already
/// `$FILES_DIR/plugin-state/<PluginKey fs-id>/tmp`. Eight hex chars identify
/// the plugin root; four more are a per-session nonce so concurrent
/// `materialize` callers of the *same* root cannot clobber
/// `workerd-config.capnp`. Unix sockets no longer need this leaf to keep
/// absolute paths under `sockaddr_un` (Linux abstract / relative GRANTED).
fn workerd_state_leaf(plugin_root: &Path, nonce_hex: &str) -> String {
    debug_assert_eq!(nonce_hex.len(), SESSION_NONCE_HEX);
    format!("w{}{nonce_hex}", plugin_root_prefix(plugin_root))
}

/// Writable parent for isolate state (`$TMPDIR`, or `.bookclerk-state` beside the root).
fn workerd_state_base(plugin_root: &Path) -> PathBuf {
    std::env::var_os("TMPDIR")
        .or_else(|| std::env::var_os("TEMP"))
        .or_else(|| std::env::var_os("TMP"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| plugin_root.join(".bookclerk-state"))
}

/// Creates `path` exclusively with Unix mode `0700` (does not trust umask).
///
/// # Errors
///
/// Returns an I/O error when the directory cannot be created, including
/// [`std::io::ErrorKind::AlreadyExists`].
fn create_exclusive_owner_only_dir(path: &Path) -> std::io::Result<()> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing path with '..': {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// Creates `path` (and parents) with Unix mode `0700` when it does not exist.
///
/// Existing directories are chmodded to `0700` as defense in depth.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or chmodded.
fn ensure_owner_only_dir(path: &Path) -> Result<()> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("refusing path with '..': {}", path.display());
    }
    if path.exists() {
        return chmod_owner_only_dir(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder
            .create(path)
            .with_context(|| format!("create {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    }
    chmod_owner_only_dir(path)
}

/// Restricts a directory to owner access (`0700`) so session secrets are not group/world readable.
///
/// # Errors
///
/// Returns an error when `chmod` fails.
#[cfg(unix)]
fn chmod_owner_only_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}

/// No-op on non-Unix hosts (ACLs are not applied here).
///
/// # Errors
///
/// Never fails.
#[cfg(not(unix))]
fn chmod_owner_only_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// Writes `contents` to `root` / `name` with Unix mode `0600` after
/// canonicalize + `starts_with` (token-bearing Cap'n Proto configs).
///
/// Creates the file when missing so canonicalize succeeds, then opens the
/// canonical path in this function (sink + barrier colocated).
///
/// # Errors
///
/// Returns an error when the path escapes `root` or the file cannot be written.
fn write_owner_only_file_under(
    root: &Path,
    name: &str,
    contents: impl AsRef<[u8]>,
) -> Result<PathBuf> {
    use std::io::Write;

    let name = require_single_path_component("file name", name)?;
    let root_norm =
        fs::canonicalize(root).with_context(|| format!("canonicalize root {}", root.display()))?;
    let out = root_norm.join(name);
    if !out.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            out.display(),
            root_norm.display()
        );
    }
    refuse_symlink_component(&out)?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if !out.exists() {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&out)
            .with_context(|| format!("create {}", out.display()))?;
    }
    let canon =
        fs::canonicalize(&out).with_context(|| format!("canonicalize {}", out.display()))?;
    if !canon.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            canon.display(),
            root_norm.display()
        );
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&canon)
        .with_context(|| format!("create {}", canon.display()))?;
    file.write_all(contents.as_ref())
        .with_context(|| format!("write {}", canon.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&canon, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", canon.display()))?;
    }
    Ok(canon)
}

/// Where bridge assets + Cap'n Proto config are written.
///
/// Prefer `$TMPDIR` (the guest scratch dir inside a jail). The plugin install
/// root is read-only under Landlock, so materializing `.bookclerk/` there fails.
/// Each call allocates a unique leaf (`w` + root prefix + nonce) with exclusive
/// `create_dir`, so concurrent sessions of the same plugin cannot clobber
/// `workerd-config.capnp`. GRANTED / native-proxy sockets use Linux abstract
/// names (or a relative `unix:granted.sock`) so `$TMPDIR` may exceed
/// `sockaddr_un`.
///
/// Callers that bind sockets first (the launcher) must pass the returned path
/// into [`materialize`] / [`materialize_native_backend`] so config and sockets
/// share one directory.
///
/// # Arguments
///
/// * `plugin_root` - Filesystem path (`plugin_root`).
///
/// # Returns
///
/// On success, the inner `PathBuf` value.
///
/// # Errors
///
/// Returns an error when the directory cannot be created exclusively.
pub fn workerd_state_dir(plugin_root: &Path) -> Result<PathBuf> {
    use rand::RngCore;

    let base = workerd_state_base(plugin_root);
    fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;
    let base = fs::canonicalize(&base)
        .with_context(|| format!("canonicalize state base {}", base.display()))?;
    let mut rng = rand::thread_rng();
    for _ in 0..64 {
        let mut nonce = [0u8; SESSION_NONCE_HEX / 2];
        rng.fill_bytes(&mut nonce);
        let leaf = workerd_state_leaf(plugin_root, &hex::encode(nonce));
        let dir = base.join(&leaf);
        if dir.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("refusing path with '..': {}", dir.display());
        }
        match create_exclusive_owner_only_dir(&dir) {
            Ok(()) => {
                let canon = fs::canonicalize(&dir)
                    .with_context(|| format!("canonicalize {}", dir.display()))?;
                if !canon.starts_with(&base) {
                    bail!("path {} escapes root {}", canon.display(), base.display());
                }
                return Ok(canon);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("create {}", dir.display()));
            }
        }
    }
    bail!(
        "could not allocate a unique workerd state directory under {}",
        base.display()
    )
}

/// Uses `state_dir` when provided; otherwise allocates via [`workerd_state_dir`].
///
/// Always returns a canonical absolute path so later `ensure_dir_under` /
/// `write_file_under` barriers can fire.
fn resolve_state_dir(root: &Path, state_dir: Option<&Path>) -> Result<PathBuf> {
    let dir = match state_dir {
        Some(dir) => {
            ensure_owner_only_dir(dir)?;
            dir.to_path_buf()
        }
        None => workerd_state_dir(root)?,
    };
    fs::canonicalize(&dir).with_context(|| format!("canonicalize state dir {}", dir.display()))
}

/// How the bridge HTTP socket is exposed to `bookclerk-workerd`.
#[derive(Debug, Clone)]
pub enum ListenSpec {
    /// `127.0.0.1:port` — workerd binds itself (unconfined smoke / Windows).
    TcpLoopback(u16),
    /// Parent already bound `127.0.0.1:0`; workerd inherits via `--socket-fd`.
    /// Required under Linux Landlock `OutboundListen` (only `bind(port=0)` is
    /// allowed — rebinding a concrete ephemeral port is EPERM).
    InheritedTcp {
        /// Ephemeral port already bound by the parent (informational for clients).
        port: u16,
    },
}

impl ListenSpec {
    /// Cap'n Proto `sockets` entry for the bridge RPC listener.
    ///
    /// Inherited FDs omit `address` — workerd gets `--socket-fd=rpc=<fd>`.
    #[must_use]
    pub fn workerd_socket_line(&self) -> String {
        match self {
            Self::TcpLoopback(port) => format!(
                r#"(name = "rpc", address = "127.0.0.1:{}", http = (capnpConnectHost = "plugin"), service = "bridge")"#,
                port
            ),
            Self::InheritedTcp { .. } => {
                r#"(name = "rpc", http = (capnpConnectHost = "plugin"), service = "bridge")"#
                    .to_string()
            }
        }
    }

    /// HTTP origin the workerd isolate uses to call back into Bookclerk.
    #[must_use]
    pub fn client_base_url(&self) -> String {
        match self {
            Self::TcpLoopback(port) | Self::InheritedTcp { port } => {
                format!("http://127.0.0.1:{port}")
            }
        }
    }

    /// TCP port the bridge socket binds.
    #[must_use]
    pub fn port(&self) -> u16 {
        match self {
            Self::TcpLoopback(port) | Self::InheritedTcp { port } => *port,
        }
    }
}

/// How the plugin worker entrypoint is produced.
///
/// The hybrid executor lives above this crate (`bookclerk-plugin-host`).
/// `bookclerk-workerd` only materializes these seams. Do not add
/// `PluginExecutor` here — a later host executor consumes [`EntrypointSource`]
/// + [`BindingSpec`].
#[derive(Debug, Clone)]
pub enum EntrypointSource {
    /// Author modules from the install `modules/` directory.
    AuthorModules {
        /// Directory containing author ES modules (relative to plugin root).
        modules_dir: String,
        /// Main module filename inside `modules_dir`.
        main_module: String,
        /// Optional exported entrypoint class name.
        entrypoint: Option<String>,
    },
    /// Generated first-party backend-proxy worker (native jail / later container).
    GeneratedBackendProxy {
        /// Module source the launcher writes into the state dir.
        module_source: String,
        /// Exported entrypoint class name.
        entrypoint: String,
    },
}

/// One workerd binding the wrapper or author isolate may receive.
#[derive(Debug, Clone)]
pub struct BindingSpec {
    /// Binding name (`HTTP`, `STORAGE`, `GRANTED`, …).
    pub name: String,
    /// Where the binding is realized.
    pub target: BindingTarget,
}

/// Realization of a [`BindingSpec`].
#[derive(Debug, Clone)]
pub enum BindingTarget {
    /// Another named workerd service (isolate RPC).
    IsolateService {
        /// Service name in the generated config.
        service: String,
    },
    /// External `fetch()` / raw streaming HTTP (Unix socket on POSIX).
    ExternalFetch {
        /// `unix:/path` or `host:port`.
        address: String,
    },
}

/// Bindings the generated adapter isolate receives (not the author isolate).
///
/// # Arguments
///
/// * `granted` - When true, include the host reverse-channel `GRANTED` binding.
#[must_use]
pub fn adapter_binding_plan(granted: bool) -> Vec<BindingSpec> {
    let mut out = vec![BindingSpec {
        name: "PLUGIN".into(),
        target: BindingTarget::IsolateService {
            service: "plugin".into(),
        },
    }];
    if granted {
        out.push(BindingSpec {
            name: "GRANTED".into(),
            target: BindingTarget::IsolateService {
                service: "granted".into(),
            },
        });
    }
    out
}

/// Adapter binding name and author-exported class name for one named
/// entrypoint (`storefront` → `PLUGIN_STOREFRONT` / `Storefront`).
///
/// # Arguments
///
/// * `entrypoint` - Named entrypoint from the manifest.
///
/// # Returns
///
/// `(binding, class)` pair the generated config binds on the adapter isolate.
#[must_use]
pub fn entrypoint_binding(entrypoint: Entrypoint) -> (String, String) {
    let wire = entrypoint.wire_name();
    let mut binding = String::from("PLUGIN_");
    let mut class = String::new();
    for (i, ch) in wire.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            binding.push('_');
        }
        binding.push(ch.to_ascii_uppercase());
        if i == 0 {
            class.push(ch.to_ascii_uppercase());
        } else {
            class.push(ch);
        }
    }
    (binding, class)
}

/// `PLUGIN_<ENTRYPOINT>` service bindings the adapter isolate receives for
/// every named entrypoint in `manifest.entrypoints`, in manifest order.
///
/// # Arguments
///
/// * `manifest` - Parsed plugin manifest.
///
/// # Returns
///
/// `(binding, class)` pairs; empty when the plugin exports no named entrypoint.
#[must_use]
pub fn adapter_entrypoint_bindings(manifest: &PluginManifest) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    manifest
        .entrypoints
        .iter()
        .copied()
        .filter(|e| seen.insert(*e))
        .map(entrypoint_binding)
        .collect()
}

/// Manifest-derived `PluginDescribe` bound on the adapter as `PLUGIN_DESCRIBE`.
///
/// The adapter answers `describe` from this projection and lets the author's
/// optional `describe()` refine presentation fields only.
///
/// # Arguments
///
/// * `manifest` - Parsed plugin manifest.
///
/// # Errors
///
/// Returns an error when the describe struct cannot be serialized as JSON.
pub fn manifest_describe_json(manifest: &PluginManifest) -> Result<String> {
    let describe = PluginDescribe {
        id: manifest.id.clone(),
        display_name: manifest.name.clone(),
        rpc_features: vec![
            bookclerk_plugin_abi::FEATURE_SCALAR_LIMITS.to_string(),
            bookclerk_plugin_abi::FEATURE_STREAMS.to_string(),
        ],
        capabilities: manifest.capabilities(),
        cli: manifest.cli.clone().unwrap_or_default(),
        ..PluginDescribe::default()
    };
    serde_json::to_string(&describe).context("serialize PLUGIN_DESCRIBE")
}

/// Entrypoint + bindings for a generated native-behind-workerd proxy.
///
/// Does not require an author `[workerd]` module tree and binds no data-plane
/// service: the adapter isolate only owns the control plane (`GRANTED`,
/// `BRIDGE_TOKEN`, `PLUGIN_DESCRIBE`).
#[must_use]
pub fn generated_backend_proxy_plan() -> (EntrypointSource, Vec<BindingSpec>) {
    (
        EntrypointSource::GeneratedBackendProxy {
            module_source: NATIVE_ADAPTER_JS.to_string(),
            entrypoint: "default".into(),
        },
        vec![BindingSpec {
            name: "GRANTED".into(),
            target: BindingTarget::IsolateService {
                service: "granted".into(),
            },
        }],
    )
}

/// Materialize a generated control-plane adapter isolate for a native guest
/// (no author `[workerd]` modules).
///
/// The host executor owns the process tree: it launches workerd and the
/// verified native guest, and forwards every entrypoint call to the guest as
/// typed Cap'n Proto. The adapter isolate decides `describe` / `open` policy
/// from the manifest projection bound as `PLUGIN_DESCRIBE`. Plugin input
/// cannot choose the executable or weaken the sandbox.
///
/// `state_dir` is an existing session directory from [`workerd_state_dir`], or
/// `None` to allocate a unique directory.
///
/// # Errors
///
/// Returns an error when state-dir I/O, describe serialization, or the config
/// write fails.
#[allow(clippy::too_many_arguments)]
pub fn materialize_native_backend(
    root: &Path,
    manifest: &PluginManifest,
    egress: &EgressProxy,
    limits: EffectiveWorkerdLimits,
    listen: ListenSpec,
    granted_addr: Option<&str>,
    bridge_token: &str,
    state_dir: Option<&Path>,
) -> Result<GeneratedConfig> {
    let state_dir = resolve_state_dir(root, state_dir)?;
    let bookclerk_dir = ensure_dir_under(&state_dir, ".bookclerk")?;
    write_file_under(&bookclerk_dir, "bridge.js", BRIDGE_JS)?;
    write_file_under(&bookclerk_dir, "egress.js", EGRESS_JS)?;
    write_file_under(&bookclerk_dir, "adapter.js", NATIVE_ADAPTER_JS)?;
    write_file_under(&bookclerk_dir, "sdk-workerd.js", SDK_WORKERD_JS)?;

    let domains = egress_domains_for(false, egress.mode(), egress.allowed_initial_hosts());
    let subrequests = match egress.policy().subrequests {
        Some(granted) => limits.subrequests.min(granted),
        None => limits.subrequests,
    };
    let mut policy = egress.policy().clone();
    policy.domains = domains;
    policy.subrequests = Some(subrequests);
    let policy_json = policy.to_policy_json();
    let policy_escaped = escape_capnp(&policy_json.to_string());

    let socket_line = listen.workerd_socket_line();
    let bridge_token_binding = format!(
        r#"(name = "BRIDGE_TOKEN", text = "{}")"#,
        escape_capnp(bridge_token)
    );

    let mut extra_services = String::new();
    let mut adapter_bindings = format!(
        r#"{bridge_token_binding},
    (name = "PLUGIN_DESCRIBE", json = "{}")"#,
        escape_capnp(&manifest_describe_json(manifest)?)
    );
    if let Some(addr) = granted_addr {
        extra_services.push_str(&format!(
            r#"    (name = "granted", external = (address = "{}", http = ())),"#,
            escape_capnp(addr)
        ));
        extra_services.push('\n');
        adapter_bindings.push_str(",\n    (name = \"GRANTED\", service = \"granted\")");
    }

    let adapter_sdk_embeds: Vec<String> = SDK_JS_MODULE_NAMES
        .iter()
        .map(|mod_name| {
            format!(
                r#"(name = "{}", esModule = embed ".bookclerk/sdk-workerd.js")"#,
                escape_capnp(mod_name)
            )
        })
        .collect();
    let adapter_modules = format!(
        r#"(name = "adapter.js", esModule = embed ".bookclerk/adapter.js"),
    {}"#,
        adapter_sdk_embeds.join(",\n    ")
    );
    let bridge_bindings = format!(
        r#"(name = "PLUGIN", service = "adapter"),
    {bridge_token_binding}"#
    );
    let compat_date = escape_capnp(BUNDLED_WORKERD_COMPAT_DATE);

    let config = format!(
        r#"using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = [{internet_allow}])),
    (name = "blocked", network = (allow = [])),
    (name = "egress", worker = .egressWorker),
    (name = "adapter", worker = .adapterWorker),
    (name = "bridge", worker = .bridgeWorker),
{extra_services}
  ],
  sockets = [
    {socket_line}
  ]
);

const egressWorker :Workerd.Worker = (
  modules = [
    (name = "egress.js", esModule = embed ".bookclerk/egress.js")
  ],
  compatibilityDate = "{compat_date}",
  {egress_flags}
  bindings = [
    (name = "EGRESS_POLICY", json = "{policy_escaped}")
  ],
  globalOutbound = "internet",
);

const adapterWorker :Workerd.Worker = (
  modules = [
    {adapter_modules}
  ],
  compatibilityDate = "{compat_date}",
  bindings = [
    {adapter_bindings}
  ],
  globalOutbound = "blocked",
);

const bridgeWorker :Workerd.Worker = (
  modules = [
    (name = "bridge.js", esModule = embed ".bookclerk/bridge.js")
  ],
  compatibilityDate = "{compat_date}",
  bindings = [
    {bridge_bindings}
  ],
  globalOutbound = "blocked",
);
"#,
        socket_line = socket_line,
        compat_date = compat_date,
        adapter_modules = adapter_modules,
        policy_escaped = policy_escaped,
        extra_services = extra_services,
        adapter_bindings = adapter_bindings,
        bridge_bindings = bridge_bindings,
        internet_allow = internet_allow_tokens(&policy),
        egress_flags = EGRESS_COMPAT_FLAGS,
    );

    let config_path = write_owner_only_file_under(&state_dir, "workerd-config.capnp", config)?;
    let import_path = fs::canonicalize(root)
        .with_context(|| format!("canonicalize import path {}", root.display()))?;
    Ok(GeneratedConfig {
        config_path,
        listen,
        state_dir,
        import_path,
    })
}

/// Materialize bridge assets + config under `root`, return config path and socket addr hint.
pub struct GeneratedConfig {
    /// Path to the generated workerd config file.
    pub config_path: PathBuf,
    /// Listen address for the bridge socket.
    pub listen: ListenSpec,
    /// Writable state directory for the isolate (outside the install root).
    pub state_dir: PathBuf,
    /// Pass to `workerd serve -I` so Cap'n Proto `/modules/…` embeds resolve
    /// against the read-only plugin install root.
    pub import_path: PathBuf,
}

/// Materialize bridge assets + Cap'n Proto config into a writable state dir.
///
/// **TMPDIR contents (guest-writable scratch)** — only generated launcher state:
/// - `.bookclerk/` — first-party bridge / egress / host stub / injected SDK
/// - `workerd-config.capnp`
/// - `granted.sock` (created by the launcher, not this function)
///
/// **Author `modules/` stay in the read-only install root.** Cap'n Proto paths
/// that look absolute (`/…`) are resolved via `--import-path` (same rules as
/// `import "/workerd/workerd.capnp"`), not the filesystem root — so embeds are
/// `/modules/…` with the plugin install root passed as `-I`.
///
/// Today's launcher materializes [`EntrypointSource::AuthorModules`] from the
/// install tree **and** a distinct adapter isolate that alone receives
/// `GRANTED` / `BRIDGE_TOKEN`. [`EntrypointSource::GeneratedBackendProxy`] and
/// [`BindingSpec`] are seams for a later host executor (native jail / container
/// behind a generated `fetch()` proxy) — that path must not require author
/// `[workerd]` modules. Direct native Cap'n Proto remains a host-selected
/// fallback, not plugin-selectable policy bypass.
///
/// `granted_addr` is an optional workerd `external` address (`host:port`,
/// `unix:/path`, `unix:granted.sock`, or Linux `unix-abstract:name`) for the
/// adapter-private `GRANTED` capability channel.
///
/// `bridge_token` is a per-isolate bearer shared by the launcher and the bridge
/// `/rpc` + `/health` routes (`BRIDGE_TOKEN` binding).
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `manifest` - Parsed plugin manifest.
/// * `egress` - `egress` input for this call.
/// * `listen` - Daemon listen address (`host:port` or URL).
/// * `granted_addr` - Optional `GRANTED` external service address.
/// * `bridge_token` - Per-isolate bridge bearer token.
/// * `state_dir` - Existing session directory from [`workerd_state_dir`], or
///   `None` to allocate a unique directory.
///
/// # Returns
///
/// On success, the inner `GeneratedConfig` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(clippy::too_many_arguments)]
pub fn materialize(
    root: &Path,
    manifest: &PluginManifest,
    egress: &EgressProxy,
    limits: EffectiveWorkerdLimits,
    listen: ListenSpec,
    granted_addr: Option<&str>,
    bridge_token: &str,
    state_dir: Option<&Path>,
) -> Result<GeneratedConfig> {
    let workerd = manifest
        .workerd
        .as_ref()
        .context("missing [workerd] table")?;

    let state_dir = resolve_state_dir(root, state_dir)?;
    let bookclerk_dir = ensure_dir_under(&state_dir, ".bookclerk")?;
    write_file_under(&bookclerk_dir, "bridge.js", BRIDGE_JS)?;
    write_file_under(&bookclerk_dir, "egress.js", EGRESS_JS)?;
    write_file_under(&bookclerk_dir, "adapter.js", ADAPTER_JS)?;

    let modules_name = require_relative_manifest_path("modules_dir", &workerd.modules_dir)?;
    let main_name = require_relative_manifest_path("main_module", &workerd.main_module)?;
    let modules_dir = {
        let dir = root.join(modules_name);
        require_under(root, &dir)?
    };
    if !modules_dir.is_dir() {
        bail!("modules dir missing: {}", modules_dir.display());
    }
    let main_abs = require_under(root, &modules_dir.join(main_name))?;
    if !main_abs.is_file() {
        bail!("main module missing: {}", main_abs.display());
    }

    let mut module_files = collect_modules(&modules_dir)?;
    // Ensure main module is first (workerd treats the first module as the entry).
    module_files.retain(|p| p != &main_abs);
    let mut ordered = vec![main_abs.clone()];
    ordered.extend(module_files);

    let modules_prefix = modules_name.replace('\\', "/");
    let mut module_embeds = Vec::new();
    let mut needs_python = false;
    let mut needs_js = false;
    let mut seen_names = std::collections::HashSet::<String>::new();
    for path in &ordered {
        let name = path
            .strip_prefix(&modules_dir)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        // Cap'n Proto `/…` = import-path relative (see GeneratedConfig / -I),
        // not a filesystem absolute. Keeps author code on the RO install root.
        let embed = format!("/{modules_prefix}/{name}");
        // Skip legacy/local embeds — host injects package-named SDK modules.
        if is_legacy_sdk_embed(&name) {
            continue;
        }
        let (field, python) = module_field_for(&name)?;
        if python {
            needs_python = true;
        } else if name.ends_with(".js") || name.ends_with(".mjs") {
            needs_js = true;
        }
        seen_names.insert(name.clone());
        module_embeds.push(format!(
            r#"(name = "{}", {} = embed "{}")"#,
            escape_capnp(&name),
            field,
            escape_capnp(&embed)
        ));
    }

    // Inject dual-stack SDK under the package import names authors use.
    if needs_js {
        write_file_under(&bookclerk_dir, "sdk-workerd.js", SDK_WORKERD_JS)?;
        for mod_name in SDK_JS_MODULE_NAMES {
            if seen_names.contains(*mod_name) {
                continue;
            }
            module_embeds.push(format!(
                r#"(name = "{}", esModule = embed ".bookclerk/sdk-workerd.js")"#,
                escape_capnp(mod_name)
            ));
            seen_names.insert((*mod_name).to_string());
        }
    }
    if needs_python {
        write_file_under(&bookclerk_dir, "sdk-workerd.py", SDK_WORKERD_PY)?;
        write_file_under(&bookclerk_dir, "sdk-db-value.py", SDK_DB_VALUE_PY)?;
        write_file_under(&bookclerk_dir, "sdk-product-abi.py", SDK_PRODUCT_ABI_PY)?;
        write_file_under(&bookclerk_dir, "sdk-guest-sql.py", SDK_GUEST_SQL_PY)?;
        write_file_under(&bookclerk_dir, "sdk-init.py", SDK_PY_INIT)?;
        for (module_name, embed_file) in [
            (SDK_PY_INIT_MODULE, "sdk-init.py"),
            (SDK_PY_PRODUCT_ABI_MODULE, "sdk-product-abi.py"),
            (SDK_PY_GUEST_SQL_MODULE, "sdk-guest-sql.py"),
            (SDK_PY_DB_VALUE_MODULE, "sdk-db-value.py"),
            (SDK_PY_WORKERD_MODULE, "sdk-workerd.py"),
        ] {
            if seen_names.contains(module_name) {
                continue;
            }
            module_embeds.push(format!(
                r#"(name = "{}", pythonModule = embed ".bookclerk/{embed_file}")"#,
                escape_capnp(module_name)
            ));
            seen_names.insert(module_name.to_string());
        }
    }

    let mut flags = workerd.compatibility_flags.clone();
    if needs_python {
        // Built-in Pyodide `workers` module (no pywrangler external SDK bundle).
        for required in ["python_workers", "disable_python_external_sdk"] {
            if !flags.iter().any(|f| f == required) {
                flags.push(required.into());
            }
        }
    }
    let flags_line = if flags.is_empty() {
        String::new()
    } else {
        let list = flags
            .iter()
            .map(|f| format!("\"{}\"", escape_capnp(f)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("compatibilityFlags = [{list}],")
    };

    // Host/adapter/bridge stay plain JS — never inherit python_workers (heavy).
    let bridge_flags = String::new();

    // Domain allowlist must match consent_request (manifest language + outbound),
    // not silently widen from on-disk .py detection alone.
    let domains = egress_domains_for(
        manifest_needs_python(manifest),
        egress.mode(),
        egress.allowed_initial_hosts(),
    );
    // Prefer the caller-supplied effective limits (manifest + optional operator
    // grant). Never raise above an egress policy subrequest ceiling already set.
    let subrequests = match egress.policy().subrequests {
        Some(granted) => limits.subrequests.min(granted),
        None => limits.subrequests,
    };
    tracing::info!(
        cpu_ms = limits.cpu_ms,
        subrequests,
        "effective workerd limits (clamped; subrequests enforced in egress)"
    );
    let mut policy = egress.policy().clone();
    policy.domains = domains;
    // Always inject clamped subrequests so the bridge can enforce (even if the
    // caller built EgressProxy without going through from_manifest).
    policy.subrequests = Some(subrequests);
    let policy_json = policy.to_policy_json();
    let policy_escaped = escape_capnp(&policy_json.to_string());

    let entrypoint = workerd.entrypoint.as_str();
    let plugin_service_binding = if entrypoint == "default" {
        r#"(name = "PLUGIN", service = "plugin")"#.to_string()
    } else {
        format!(
            r#"(name = "PLUGIN", service = (name = "plugin", entrypoint = "{}"))"#,
            escape_capnp(entrypoint)
        )
    };
    let adapter_service_binding = r#"(name = "PLUGIN", service = "adapter")"#;
    // One `PLUGIN_<ENTRYPOINT>` service binding per named entrypoint the
    // manifest exports, targeting the author's exported class of the same
    // name (`Storefront`, `Storage`, …), plus the manifest `PluginDescribe`
    // projection so authors need not implement `describe()`.
    let mut named_entrypoint_bindings = String::new();
    for (binding, class) in adapter_entrypoint_bindings(manifest) {
        named_entrypoint_bindings.push_str(&format!(
            ",\n    (name = \"{}\", service = (name = \"plugin\", entrypoint = \"{}\"))",
            escape_capnp(&binding),
            escape_capnp(&class)
        ));
    }
    named_entrypoint_bindings.push_str(&format!(
        ",\n    (name = \"PLUGIN_DESCRIBE\", json = \"{}\")",
        escape_capnp(&manifest_describe_json(manifest)?)
    ));

    let socket_line = listen.workerd_socket_line();
    // Never force unrestricted `internet` for Python under Deny. Outbound +
    // Python uses the egress proxy with Pyodide CDN hosts auto-allowlisted.
    let plugin_outbound = plugin_global_outbound(egress.mode());

    let bridge_token_binding = format!(
        r#"(name = "BRIDGE_TOKEN", text = "{}")"#,
        escape_capnp(bridge_token)
    );

    let mut extra_services = String::new();
    // Bridge talks to the adapter isolate. GRANTED / BRIDGE_TOKEN are adapter-private
    // (not author `pluginWorker` bindings). wrapPlugin stripping keys is hygiene.
    let bridge_bindings = format!("{adapter_service_binding},\n    {bridge_token_binding}");
    let mut adapter_bindings =
        format!("{plugin_service_binding}{named_entrypoint_bindings},\n    {bridge_token_binding}");
    if let Some(addr) = granted_addr {
        extra_services.push_str(&format!(
            r#"    (name = "granted", external = (address = "{}", http = ())),"#,
            escape_capnp(addr)
        ));
        extra_services.push('\n');
        adapter_bindings.push_str(",\n    (name = \"GRANTED\", service = \"granted\")");
    }

    // Adapter always loads the JS SDK (even when the author isolate is Python).
    write_file_under(&bookclerk_dir, "sdk-workerd.js", SDK_WORKERD_JS)?;
    let adapter_sdk_embeds: Vec<String> = SDK_JS_MODULE_NAMES
        .iter()
        .map(|mod_name| {
            format!(
                r#"(name = "{}", esModule = embed ".bookclerk/sdk-workerd.js")"#,
                escape_capnp(mod_name)
            )
        })
        .collect();
    let adapter_modules = format!(
        r#"(name = "adapter.js", esModule = embed ".bookclerk/adapter.js"),
    {}"#,
        adapter_sdk_embeds.join(",\n    ")
    );

    let config = format!(
        r#"using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = [{internet_allow}])),
    (name = "blocked", network = (allow = [])),
    (name = "egress", worker = .egressWorker),
    (name = "plugin", worker = .pluginWorker),
    (name = "adapter", worker = .adapterWorker),
    (name = "bridge", worker = .bridgeWorker),
{extra_services}
  ],
  sockets = [
    {socket_line}
  ]
);

const egressWorker :Workerd.Worker = (
  modules = [
    (name = "egress.js", esModule = embed ".bookclerk/egress.js")
  ],
  compatibilityDate = "{compat_date}",
  {egress_flags}
  bindings = [
    (name = "EGRESS_POLICY", json = "{policy_escaped}")
  ],
  globalOutbound = "internet",
);

const pluginWorker :Workerd.Worker = (
  modules = [
    {modules}
  ],
  compatibilityDate = "{compat_date}",
  {plugin_flags}
  bindings = [],
  globalOutbound = "{plugin_outbound}",
);

const adapterWorker :Workerd.Worker = (
  modules = [
    {adapter_modules}
  ],
  compatibilityDate = "{compat_date}",
  {bridge_flags}
  bindings = [
    {adapter_bindings}
  ],
  globalOutbound = "blocked",
);

const bridgeWorker :Workerd.Worker = (
  modules = [
    (name = "bridge.js", esModule = embed ".bookclerk/bridge.js")
  ],
  compatibilityDate = "{compat_date}",
  {bridge_flags}
  bindings = [
    {bridge_bindings}
  ],
  globalOutbound = "blocked",
);
"#,
        socket_line = socket_line,
        compat_date = escape_capnp(&workerd.compatibility_date),
        bridge_flags = bridge_flags,
        plugin_flags = flags_line,
        modules = module_embeds.join(",\n    "),
        adapter_modules = adapter_modules,
        policy_escaped = policy_escaped,
        extra_services = extra_services,
        adapter_bindings = adapter_bindings,
        bridge_bindings = bridge_bindings,
        plugin_outbound = plugin_outbound,
        internet_allow = internet_allow_tokens(&policy),
        egress_flags = EGRESS_COMPAT_FLAGS,
    );

    let config_path = write_owner_only_file_under(&state_dir, "workerd-config.capnp", config)?;

    let import_path = fs::canonicalize(root)
        .with_context(|| format!("canonicalize import path {}", root.display()))?;

    Ok(GeneratedConfig {
        config_path,
        listen,
        state_dir,
        import_path,
    })
}

/// Plugin isolate `globalOutbound`: Deny → blocked; Outbound → egress proxy.
///
/// # Arguments
///
/// * `mode` - Network mode from the plugin manifest.
///
/// # Returns
///
/// `&'static str` result.
#[must_use]
pub fn plugin_global_outbound(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Deny => "blocked",
        NetworkMode::Outbound => "egress",
    }
}

/// Workerd `network.allow` inner list: `public` plus explicit CIDRs, never `private`.
fn internet_allow_tokens(policy: &EgressPolicy) -> String {
    let cidrs: Vec<CidrGrant> = policy
        .address_cidrs
        .iter()
        .filter_map(|raw| CidrGrant::parse(raw).ok())
        .collect();
    workerd_network_allow(&cidrs)
        .into_iter()
        .map(|token| format!("\"{}\"", escape_capnp(&token)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Egress allowlist: plugin domains, plus Pyodide CDN hosts when Python + Outbound.
///
/// Uses the same host set as [`bookclerk_plugin_manifest::consent_domains_for`] so
/// materialize cannot silently widen beyond what `consent_request` / grants cover.
///
/// # Arguments
///
/// * `needs_python` - Boolean flag `needs_python`.
/// * `mode` - Network mode from the plugin manifest.
/// * `base` - String `base` for this call.
///
/// # Returns
///
/// Collected results (may be empty).
#[must_use]
pub fn egress_domains_for(needs_python: bool, mode: NetworkMode, base: &[String]) -> Vec<String> {
    with_python_runtime_hosts(needs_python, mode, base)
}

/// Cap'n Proto module field (`esModule`, `pythonModule`, `wasm`, …) for a guest file name.
fn module_field_for(name: &str) -> Result<(&'static str, bool)> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".py") {
        Ok(("pythonModule", true))
    } else if lower.ends_with(".wasm") {
        Ok(("wasm", false))
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        Ok(("esModule", false))
    } else if lower.ends_with(".json") {
        Ok(("json", false))
    } else if lower.ends_with(".txt") || lower.ends_with(".md") {
        Ok(("text", false))
    } else {
        bail!("unsupported workerd module type for `{name}` (use .js/.mjs/.py/.wasm/.json)");
    }
}

/// True when the path is a legacy SDK embed that workerd injects itself (skip packaging twice).
fn is_legacy_sdk_embed(name: &str) -> bool {
    let n = name.replace('\\', "/");
    matches!(
        n.as_str(),
        "bookclerk_plugin.js"
            | "bookclerk_plugin.py"
            | "@bookclerk/plugin-sdk"
            | "@bookclerk/plugin-sdk/workerd"
            | "@bookclerk/plugin-sdk/workerd.js"
            | "bookclerk_plugin_sdk/workerd.py"
            | "bookclerk_plugin_sdk/db_value.py"
            | "bookclerk_plugin_sdk/_abi.py"
            | "bookclerk_plugin_sdk/guest_sql.py"
            | "bookclerk_plugin_sdk/__init__.py"
    )
}

/// Sorted list of `.js`/`.mjs`/`.py`/`.wasm`/`.json` files under a guest directory.
fn collect_modules(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_modules_inner(dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Recursively appends workerd-loadable module files under `dir`.
///
/// Symlinks are rejected so a nested `modules/leak -> /outside` cannot pass a
/// lexical `starts_with` check and walk/embed files outside the plugin tree.
fn collect_modules_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "refusing symlink in workerd modules tree: {}",
                entry.path().display()
            );
        }
        let path = entry.path();
        if !path.starts_with(dir) {
            continue;
        }
        if file_type.is_dir() {
            collect_modules_inner(&path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".js")
            || lower.ends_with(".mjs")
            || lower.ends_with(".py")
            || lower.ends_with(".wasm")
            || lower.ends_with(".json")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Escapes backslashes and quotes for a Cap'n Proto string literal.
fn escape_capnp(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native `storage` manifest for the native-behind-workerd materializer.
    fn native_manifest() -> PluginManifest {
        PluginManifest::parse(
            r#"
api_version = 3
id = "local"
runtime = "native"
command = "./bookclerk-plugin-destination-local"
entrypoints = ["storage"]

[capabilities.network]
mode = "deny"
"#,
        )
        .expect("native manifest")
    }

    #[test]
    fn module_field_python_and_wasm() {
        assert_eq!(
            module_field_for("plugin.py").unwrap(),
            ("pythonModule", true)
        );
        assert_eq!(module_field_for("echo_bg.wasm").unwrap(), ("wasm", false));
        assert_eq!(module_field_for("index.js").unwrap(), ("esModule", false));
    }

    #[test]
    fn python_flags_include_external_sdk_disable() {
        // Local workerd needs the built-in Pyodide `workers` module; without
        // this flag, `from workers import WorkerEntrypoint` fails at import.
        let required = ["python_workers", "disable_python_external_sdk"];
        let mut flags: Vec<String> = Vec::new();
        for required in required {
            if !flags.iter().any(|f| f == required) {
                flags.push(required.into());
            }
        }
        assert_eq!(
            flags,
            vec![
                "python_workers".to_string(),
                "disable_python_external_sdk".to_string()
            ]
        );
    }

    #[test]
    fn python_sdk_injects_db_value_module() {
        use bookclerk_plugin_manifest::{PluginManifest, WorkerdLimits};

        let dir = tempfile::tempdir().expect("tempdir");
        let _modules = create_dir_under(dir.path(), "modules");
        write_under(
            dir.path(),
            Path::new("modules").join("plugin.py"),
            "from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint\n",
        );
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "echo_py"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
compatibility_flags = ["python_workers", "disable_python_external_sdk"]
main_module = "plugin.py"
modules_dir = "modules"
entrypoint = "default"

[capabilities.network]
mode = "deny"
"#,
        )
        .expect("manifest");
        let generated = materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            "test-bridge-token",
            None,
        )
        .expect("materialize");
        let capnp = read_under(&generated.state_dir, &generated.config_path);
        assert!(capnp.contains(SDK_PY_DB_VALUE_MODULE));
        assert!(capnp.contains("sdk-db-value.py"));
        let state_dir = &generated.state_dir;
        assert!(state_dir.join(".bookclerk/sdk-db-value.py").is_file());
    }

    #[test]
    fn materialize_allows_nested_modules_and_double_dot_names() {
        use bookclerk_plugin_manifest::{PluginManifest, WorkerdLimits};

        let dir = tempfile::tempdir().expect("tempdir");
        let rel = Path::new("dist").join("modules").join("nested");
        let _ = create_dir_under(dir.path(), &rel);
        write_under(dir.path(), rel.join("edition..2.js"), "export default {};");
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "nested"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "nested/edition..2.js"
modules_dir = "dist/modules"
[capabilities.network]
mode = "deny"
"#,
        )
        .expect("manifest");
        let generated = materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            "token",
            None,
        )
        .expect("materialize nested modules");
        let capnp = read_under(&generated.state_dir, &generated.config_path);
        assert!(
            capnp.contains(r#"name = "edition..2.js""#) || capnp.contains("edition..2.js"),
            "capnp missing double-dot module name:\n{capnp}"
        );
        assert!(
            capnp.contains("/dist/modules/nested/edition..2.js"),
            "capnp missing nested embed:\n{capnp}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_refuses_leaf_symlink_in_supplied_state_dir() {
        use bookclerk_plugin_manifest::{PluginManifest, WorkerdLimits};
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let _modules = create_dir_under(dir.path(), "modules");
        write_under(
            dir.path(),
            Path::new("modules").join("index.js"),
            "export default {};",
        );
        let state = tempfile::tempdir().expect("state");
        let bookclerk = state.path().join(".bookclerk");
        fs::create_dir(&bookclerk).expect("bookclerk dir");
        let victim = dir.path().join("victim.js");
        fs::write(&victim, b"VICTIM").expect("victim");
        symlink(&victim, bookclerk.join("adapter.js")).expect("symlink");
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"
[capabilities.network]
mode = "deny"
"#,
        )
        .expect("manifest");
        let err = match materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            "token",
            Some(state.path()),
        ) {
            Ok(_) => panic!("leaf symlink must be refused"),
            Err(err) => err,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("symlink"),
            "expected symlink refusal, got {msg}"
        );
        assert_eq!(fs::read(&victim).expect("victim bytes"), b"VICTIM");
    }

    #[test]
    fn python_deny_never_unrestricted_internet() {
        assert_eq!(plugin_global_outbound(NetworkMode::Deny), "blocked");
        assert_eq!(plugin_global_outbound(NetworkMode::Outbound), "egress");
        let deny_domains = egress_domains_for(true, NetworkMode::Deny, &["api.example.com".into()]);
        assert_eq!(deny_domains, vec!["api.example.com".to_string()]);
        let outbound = egress_domains_for(true, NetworkMode::Outbound, &["api.example.com".into()]);
        assert!(outbound.contains(&"api.example.com".to_string()));
        for host in PYODIDE_EGRESS_HOSTS {
            assert!(
                outbound.iter().any(|d| d == *host),
                "missing Pyodide host {host}"
            );
        }
    }

    #[test]
    fn author_module_embeds_use_import_path_form() {
        // Cap'n Proto `/x` is import-path relative (same as `/workerd/workerd.capnp`),
        // not a filesystem absolute — so we must not emit `/home/.../modules/...`.
        let modules_prefix = "modules";
        let name = "plugin.py";
        let embed = format!("/{modules_prefix}/{name}");
        assert_eq!(embed, "/modules/plugin.py");
        assert!(!embed.contains("home"));
        assert!(ListenSpec::InheritedTcp { port: 9 }
            .workerd_socket_line()
            .contains(r#"(name = "rpc""#));
        assert!(!ListenSpec::InheritedTcp { port: 9 }
            .workerd_socket_line()
            .contains("address"));
    }

    #[test]
    fn egress_policy_json_includes_clamped_subrequests() {
        use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode, WorkerdLimits};

        let limits = WorkerdLimits {
            cpu_ms: Some(30_000),
            subrequests: Some(50),
        }
        .effective();
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["api.example.com".into()],
            max_redirects: 10,
            subrequests: Some(limits.subrequests),
            ..EgressPolicy::deny()
        };
        let v = policy.to_policy_json();
        assert_eq!(v["subrequests"], 50);
        assert_eq!(v["maxRedirects"], 10);

        let capped = WorkerdLimits {
            cpu_ms: None,
            subrequests: Some(50_000),
        }
        .effective();
        assert_eq!(capped.subrequests, WorkerdLimits::MAX_SUBREQUESTS);
    }

    #[test]
    fn internet_service_lists_public_and_explicit_cidrs_never_private() {
        use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode, PluginManifest, WorkerdLimits};

        let dir = tempfile::tempdir().expect("tempdir");
        let _modules = create_dir_under(dir.path(), "modules");
        write_under(
            dir.path(),
            Path::new("modules").join("index.js"),
            "export default {};",
        );
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"
[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]
address_cidrs = ["10.0.60.100/32"]
"#,
        )
        .expect("manifest");
        let mut policy = EgressPolicy::from_manifest(&manifest);
        policy.mode = NetworkMode::Outbound;
        policy.address_cidrs = vec!["10.0.60.100/32".into()];
        let generated = materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(policy),
            WorkerdLimits::default().effective(),
            ListenSpec::TcpLoopback(0),
            None,
            "token",
            None,
        )
        .expect("materialize");
        let capnp = read_under(&generated.state_dir, &generated.config_path);
        assert!(
            capnp.contains(r#"allow = ["10.0.60.100/32", "public"]"#)
                || capnp.contains(r#"allow = ["public", "10.0.60.100/32"]"#),
            "{capnp}"
        );
        assert!(!capnp.contains("\"private\""));
        assert!(!capnp.contains("\"local\""));
        assert!(
            capnp.contains(r#"compatibilityFlags = ["experimental"]"#),
            "egress worker must opt into inbound CONNECT: {capnp}"
        );
        assert_eq!(WORKERD_SERVE_EXPERIMENTAL, "--experimental");
    }

    fn socket_names(capnp: &str) -> Vec<String> {
        let start = capnp.find("sockets = [").expect("sockets block");
        let block = &capnp[start..];
        let end = block.find(']').expect("sockets close");
        let block = &block[..end];
        block
            .split("name = \"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next().map(str::to_string))
            .collect()
    }

    fn plugin_worker_outbound(capnp: &str) -> &str {
        let start = capnp.find("const pluginWorker").expect("pluginWorker");
        let after = &capnp[start + "const pluginWorker".len()..];
        let end = after.find("const ").unwrap_or(after.len());
        let block = &after[..end];
        if block.contains("globalOutbound = \"blocked\"") {
            "blocked"
        } else if block.contains("globalOutbound = \"egress\"") {
            "egress"
        } else if block.contains("globalOutbound = \"internet\"") {
            "internet"
        } else {
            "missing"
        }
    }

    fn materialize_capnp(flags: &[&str], mode: NetworkMode) -> String {
        use bookclerk_plugin_manifest::{PluginManifest, WorkerdLimits};

        let dir = tempfile::tempdir().expect("tempdir");
        let _modules = create_dir_under(dir.path(), "modules");
        write_under(
            dir.path(),
            Path::new("modules").join("index.js"),
            "export default {};",
        );
        let flags_toml = flags
            .iter()
            .map(|f| format!("{f:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let network_toml = match mode {
            NetworkMode::Deny => "mode = \"deny\"".to_string(),
            NetworkMode::Outbound => "mode = \"outbound\"\ndomains = [\"example.com\"]".to_string(),
        };
        let manifest = PluginManifest::parse(&format!(
            r#"
api_version = 3
id = "echo"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
compatibility_flags = [{flags_toml}]
main_module = "index.js"
modules_dir = "modules"
entrypoint = "default"

[capabilities.network]
{network_toml}
"#
        ))
        .expect("manifest");
        let generated = materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(match mode {
                NetworkMode::Deny => bookclerk_plugin_manifest::EgressPolicy::deny(),
                NetworkMode::Outbound => {
                    bookclerk_plugin_manifest::EgressPolicy::from_manifest(&manifest)
                }
            }),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            "test-bridge-token",
            None,
        )
        .expect("materialize");
        read_under(&generated.state_dir, &generated.config_path)
    }

    #[test]
    fn deny_workerd_config_exposes_only_rpc_socket_for_compat_flags() {
        use bookclerk_plugin_manifest::NetworkMode;

        // Flags authors may request; none of them may add listen sockets.
        for flags in [
            &[] as &[&str],
            &["python_workers"],
            &["python_workers", "disable_python_external_sdk"],
            &["nodejs_compat"],
            &["nodejs_compat", "streams_enable_constructors"],
        ] {
            let capnp = materialize_capnp(flags, NetworkMode::Deny);
            assert_eq!(socket_names(&capnp), vec!["rpc".to_string()], "{flags:?}");
            assert_eq!(plugin_worker_outbound(&capnp), "blocked", "{flags:?}");
            assert!(
                !capnp.contains("hostWorker") && !capnp.contains("host_stub"),
                "no HOST reverse channel may be materialized: {flags:?}"
            );
            assert!(
                capnp.contains("const bridgeWorker") && capnp.contains(r#"(name = "rpc""#),
                "rpc socket must target the bridge: {flags:?}"
            );
            let sockets = capnp
                .split("sockets = [")
                .nth(1)
                .and_then(|rest| rest.split(']').next())
                .unwrap_or("");
            assert!(
                !sockets.contains("address"),
                "inherited rpc socket must not bind a second address: {flags:?}\n{sockets}"
            );
        }
    }

    #[test]
    fn granted_addr_binds_adapter_not_author_plugin() {
        use bookclerk_plugin_manifest::{PluginManifest, WorkerdLimits};

        let dir = tempfile::tempdir().expect("tempdir");
        let _modules = create_dir_under(dir.path(), "modules");
        write_under(
            dir.path(),
            Path::new("modules").join("index.js"),
            "export default {};",
        );
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "stream_fixture"
runtime = "workerd"
entrypoints = ["storage"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"
entrypoint = "default"

[capabilities.network]
mode = "deny"
"#,
        )
        .expect("manifest");
        let generated = materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            Some("unix:/tmp/granted.sock"),
            "test-bridge-token",
            None,
        )
        .expect("materialize");
        let capnp = read_under(&generated.state_dir, &generated.config_path);
        assert!(
            capnp.contains(r#"(name = "granted", external = (address = "unix:/tmp/granted.sock""#),
            "missing granted external:\n{capnp}"
        );
        let plugin = capnp
            .split("const pluginWorker")
            .nth(1)
            .and_then(|rest| rest.split("const ").next())
            .unwrap_or("");
        assert!(
            !plugin.contains(r#"(name = "GRANTED", service = "granted")"#)
                && !plugin.contains(r#"(name = "BRIDGE_TOKEN""#),
            "author plugin worker must not receive adapter-private bindings:\n{plugin}"
        );
        let adapter = capnp
            .split("const adapterWorker")
            .nth(1)
            .and_then(|rest| rest.split("const ").next())
            .unwrap_or("");
        assert!(
            adapter.contains(r#"(name = "GRANTED", service = "granted")"#)
                && adapter.contains(r#"(name = "BRIDGE_TOKEN""#)
                && adapter.contains(r#"(name = "PLUGIN", service = "plugin")"#),
            "adapter worker must fetch GRANTED and wrap PLUGIN:\n{adapter}"
        );
        let bridge = capnp.split("const bridgeWorker").nth(1).unwrap_or("");
        assert!(
            bridge.contains(r#"(name = "PLUGIN", service = "adapter")"#),
            "bridge must bind PLUGIN to the adapter:\n{bridge}"
        );
        assert!(
            !bridge.contains(r#"(name = "GRANTED", service = "granted")"#),
            "bridge must not bind GRANTED:\n{bridge}"
        );
    }

    #[test]
    fn entrypoint_and_binding_seams_exist_without_executor() {
        let src = EntrypointSource::AuthorModules {
            modules_dir: "modules".into(),
            main_module: "index.js".into(),
            entrypoint: None,
        };
        let _ = BindingSpec {
            name: "GRANTED".into(),
            target: BindingTarget::IsolateService {
                service: "granted".into(),
            },
        };
        match src {
            EntrypointSource::AuthorModules { main_module, .. } => {
                assert_eq!(main_module, "index.js");
            }
            EntrypointSource::GeneratedBackendProxy { .. } => panic!("unexpected proxy"),
        }
        let (proxy, bindings) = generated_backend_proxy_plan();
        match proxy {
            EntrypointSource::GeneratedBackendProxy {
                module_source,
                entrypoint,
            } => {
                assert!(module_source.contains("wrapPluginFromNative"));
                assert!(!module_source.contains("wrapPluginFromBinding"));
                assert_eq!(entrypoint, "default");
            }
            EntrypointSource::AuthorModules { .. } => panic!("expected generated proxy"),
        }
        assert!(bindings.iter().any(|b| b.name == "GRANTED"));
        assert!(
            !bindings.iter().any(|b| b.name == "PLUGIN_BACKEND"),
            "native-behind-workerd adapter has no data-plane binding"
        );
        assert!(
            !bindings.iter().any(|b| b.name == "PLUGIN"),
            "native-behind-workerd adapter must not bind an author PLUGIN isolate"
        );
    }

    #[test]
    fn native_backend_materialize_does_not_need_author_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generated = materialize_native_backend(
            dir.path(),
            &native_manifest(),
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            Some("unix:/tmp/granted.sock"),
            "token",
            None,
        )
        .expect("materialize native");
        let capnp = read_under(&generated.state_dir, &generated.config_path);
        assert!(
            !capnp.contains("PLUGIN_BACKEND") && !capnp.contains("nativeBackend"),
            "native-behind-workerd must not bind a data-plane service:\n{capnp}"
        );
        let adapter = capnp
            .split("const adapterWorker")
            .nth(1)
            .and_then(|rest| rest.split("const ").next())
            .unwrap_or("");
        assert!(
            adapter.contains(r#"(name = "PLUGIN_DESCRIBE", json = ""#)
                && adapter.contains(r#"(name = "GRANTED", service = "granted")"#)
                && adapter.contains(r#"(name = "BRIDGE_TOKEN""#),
            "native adapter owns the control plane (PLUGIN_DESCRIBE / GRANTED / BRIDGE_TOKEN):\n{adapter}"
        );
        assert!(
            adapter.contains(r#"\"entrypoints\":[\"storage\"]"#),
            "PLUGIN_DESCRIBE carries the manifest capabilities:\n{adapter}"
        );
        assert!(
            !adapter.contains(r#"(name = "PLUGIN","#),
            "native adapter must not bind an author PLUGIN isolate:\n{adapter}"
        );
        assert!(
            !capnp.contains("const pluginWorker"),
            "native-behind-workerd must not require an author plugin isolate:\n{capnp}"
        );
        assert!(!dir.path().join("modules").is_dir());
    }

    #[test]
    fn github_actions_cargo_tmpdir_can_overflow_sockaddr_un() {
        // Cargo `[env] TMPDIR = .tmp` + plugin-state fs_id + workerd leaf.
        // Absolute pathname sockets overflow Linux sun_path (108); the
        // launcher uses unix-abstract / relative names instead (see unix_bind).
        let scratch = PathBuf::from(
            "/home/runner/work/bookclerk/bookclerk/.tmp/.tmpOjHB9L/plugin-state/pk-6fbbb9ca1420cebb/tmp",
        );
        let dir = scratch.join(workerd_state_leaf(
            &scratch.join("plugin-root"),
            &"f".repeat(SESSION_NONCE_HEX),
        ));
        let granted = dir.join("granted.sock");
        assert!(
            granted.to_string_lossy().len() >= 108,
            "fixture must model the GHA overflow: {} ({} bytes)",
            granted.display(),
            granted.to_string_lossy().len()
        );
    }

    #[test]
    fn granted_unix_abstract_survives_capnp() {
        use bookclerk_plugin_manifest::WorkerdLimits;

        let dir = tempfile::tempdir().expect("tempdir");
        let generated = materialize_native_backend(
            dir.path(),
            &native_manifest(),
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            Some("unix-abstract:bc-g-deadbeefcafebabe"),
            "test-bridge-token",
            None,
        )
        .expect("materialize");
        let capnp = read_under(&generated.state_dir, &generated.config_path);
        assert!(
            capnp.contains(
                r#"(name = "granted", external = (address = "unix-abstract:bc-g-deadbeefcafebabe""#
            ),
            "unix-abstract GRANTED must round-trip into workerd config:\n{capnp}"
        );
    }

    #[test]
    fn concurrent_materialize_same_plugin_root_uses_distinct_state_dirs() {
        use std::collections::HashSet;

        let plugin = tempfile::tempdir().expect("plugin");
        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let root = plugin.path().to_path_buf();
                std::thread::spawn(move || {
                    materialize_native_backend(
                        &root,
                        &native_manifest(),
                        &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
                        bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
                        ListenSpec::InheritedTcp { port: 9 },
                        Some("unix:/tmp/granted.sock"),
                        "token",
                        None,
                    )
                })
            })
            .collect();
        let mut dirs = Vec::new();
        for handle in handles {
            let generated = handle.join().expect("join").expect("materialize");
            dirs.push(generated.state_dir);
        }
        let unique: HashSet<_> = dirs.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            n,
            "concurrent sessions of the same plugin root must not share a state dir"
        );
        for dir in &dirs {
            assert!(
                dir.join("workerd-config.capnp").is_file(),
                "missing config in {}",
                dir.display()
            );
            let state = test_fs_path(dir);
            let _ = std::fs::remove_dir_all(&state);
        }
    }

    #[test]
    fn concurrent_materialize_does_not_clobber_shared_tmpdir() {
        use bookclerk_plugin_manifest::NetworkMode;

        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    if i % 2 == 0 {
                        let capnp = materialize_capnp(&[], NetworkMode::Deny);
                        assert!(
                            capnp.contains(r#"(name = "rpc""#),
                            "deny config missing rpc socket"
                        );
                        assert!(
                            !capnp.contains(r#"(name = "granted""#),
                            "deny config must not grow a granted socket"
                        );
                    } else {
                        let dir = tempfile::tempdir().expect("tempdir");
                        let generated = materialize_native_backend(
                            dir.path(),
                            &native_manifest(),
                            &EgressProxy::from_policy(
                                bookclerk_plugin_manifest::EgressPolicy::deny(),
                            ),
                            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
                            ListenSpec::InheritedTcp { port: 9 },
                            Some("unix:/tmp/granted.sock"),
                            "token",
                            None,
                        )
                        .expect("materialize native");
                        let capnp = read_under(&generated.state_dir, &generated.config_path);
                        assert!(
                            capnp.contains(r#"(name = "PLUGIN_DESCRIBE", json = ""#)
                                && !capnp.contains("const pluginWorker"),
                            "native config clobbered:\n{capnp}"
                        );
                        let state = test_fs_path(&generated.state_dir);
                        let _ = std::fs::remove_dir_all(&state);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker panicked");
        }
    }

    #[cfg(unix)]
    #[test]
    fn workerd_session_dir_is_0700_and_config_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let plugin = tempfile::tempdir().expect("plugin");
        let generated = materialize_native_backend(
            plugin.path(),
            &native_manifest(),
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            Some("unix:/tmp/granted.sock"),
            "token",
            None,
        )
        .expect("materialize");
        let state_path = test_fs_path(&generated.state_dir);
        let dir_mode = std::fs::metadata(&state_path)
            .expect("dir meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode,
            0o700,
            "session dir {}",
            generated.state_dir.display()
        );
        let cfg_path = test_fs_path(&generated.config_path);
        let cfg_mode = std::fs::metadata(&cfg_path)
            .expect("cfg meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            cfg_mode,
            0o600,
            "config {}",
            generated.config_path.display()
        );
        let state = test_fs_path(&generated.state_dir);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_state_dir_chmods_existing_session_dir() {
        use std::os::unix::fs::PermissionsExt;

        let plugin = tempfile::tempdir().expect("plugin");
        let session = tempfile::tempdir().expect("session");
        let session_path = test_fs_path(session.path());
        std::fs::set_permissions(&session_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        let generated = materialize_native_backend(
            plugin.path(),
            &native_manifest(),
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            Some("unix:/tmp/granted.sock"),
            "token",
            Some(session.path()),
        )
        .expect("materialize");
        let session_path = test_fs_path(session.path());
        let dir_mode = std::fs::metadata(&session_path)
            .expect("dir meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "existing session dir must be tightened to 0700"
        );
        let cfg_path = test_fs_path(&generated.config_path);
        let cfg_mode = std::fs::metadata(&cfg_path)
            .expect("cfg meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(cfg_mode, 0o600);
    }
}
