//! Generate a real workerd Cap'n Proto config for one plugin isolate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::{
    manifest_needs_python, with_python_runtime_hosts, EffectiveWorkerdLimits, NetworkMode,
    PluginManifest,
};

use crate::egress::EgressProxy;
use crate::pin::BUNDLED_WORKERD_COMPAT_DATE;

/// Host↔isolate RPC bridge script materialized into the workerd state dir.
const BRIDGE_JS: &str = include_str!("../bridge/bridge.js");
/// Isolate-side egress proxy that enforces the operator domain grant.
const EGRESS_JS: &str = include_str!("../bridge/egress.js");
/// Stub `host` module injected so guest JS can call host RPCs inside the isolate.
const HOST_STUB_JS: &str = include_str!("../bridge/host_stub.js");
/// Injected as `@bookclerk/plugin-sdk` + `@bookclerk/plugin-sdk/workerd`.
const SDK_WORKERD_JS: &str = include_str!("../../../packages/plugin-sdk/embed/bookclerk_plugin.js");
/// Injected as `bookclerk_plugin_sdk/workerd.py`.
const SDK_WORKERD_PY: &str =
    include_str!("../../../packages/plugin-sdk-python/src/bookclerk_plugin_sdk/workerd.py");
/// First-party adapter isolate: owns GRANTED / BRIDGE_TOKEN (author `PLUGIN`).
const ADAPTER_JS: &str = r#"import { wrapV2PluginFromBinding } from "@bookclerk/plugin-sdk/workerd";
export default wrapV2PluginFromBinding();
"#;
/// Generated native-behind-workerd adapter: `PLUGIN_BACKEND` only (no author isolate).
const NATIVE_ADAPTER_JS: &str = r#"import { wrapV2PluginFromNative } from "@bookclerk/plugin-sdk/workerd";
export default wrapV2PluginFromNative();
"#;
/// Sparse `bookclerk_plugin_sdk/__init__.py` pointing authors at the workerd guest SDK.
const SDK_PY_INIT: &str = concat!(
    "\"\"\"Bookclerk plugin SDK (workerd isolate).\n\n",
    "Use: from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js\n\n",
    "Native guests use Rust serve() / PluginRoot instead.\n",
    "\"\"\"\n"
);

/// Package import names authors use for the workerd BookclerkPlugin.
pub const SDK_JS_MODULE_NAMES: &[&str] =
    &["@bookclerk/plugin-sdk/workerd", "@bookclerk/plugin-sdk"];
/// Python package path for `from bookclerk_plugin_sdk.workerd import …`.
pub const SDK_PY_WORKERD_MODULE: &str = "bookclerk_plugin_sdk/workerd.py";
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
/// `$FILES_DIR/plugins/<id>/tmp`, so this leaf must stay short. Eight hex
/// chars identify the plugin root; four more are a per-session nonce so
/// concurrent `materialize` callers of the *same* root cannot clobber
/// `workerd-config.capnp` or unix sockets.
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

/// Writes `contents` and sets Unix mode `0600` (token-bearing files such as Cap'n Proto config).
///
/// # Errors
///
/// Returns an error when the file cannot be created or written.
fn write_owner_only_file(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    use std::io::Write;

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents.as_ref())
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

/// Where bridge assets + Cap'n Proto config are written.
///
/// Prefer `$TMPDIR` (the guest scratch dir inside a jail). The plugin install
/// root is read-only under Landlock, so materializing `.bookclerk/` there fails.
/// Each call allocates a unique leaf (`w` + root prefix + nonce) with exclusive
/// `create_dir`, so concurrent sessions of the same plugin cannot clobber
/// `workerd-config.capnp`. Unix sockets (`granted.sock` / `notify.sock`) still
/// fit in `sockaddr_un`.
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
    let mut rng = rand::thread_rng();
    for _ in 0..64 {
        let mut nonce = [0u8; SESSION_NONCE_HEX / 2];
        rng.fill_bytes(&mut nonce);
        let dir = base.join(workerd_state_leaf(plugin_root, &hex::encode(nonce)));
        match create_exclusive_owner_only_dir(&dir) {
            Ok(()) => return Ok(dir),
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
fn resolve_state_dir(root: &Path, state_dir: Option<&Path>) -> Result<PathBuf> {
    match state_dir {
        Some(dir) => {
            ensure_owner_only_dir(dir)?;
            Ok(dir.to_path_buf())
        }
        None => workerd_state_dir(root),
    }
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

    /// TCP port the isolate or notify listener binds.
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
    /// Binding name (`HTTP`, `STORAGE`, `PLUGIN_BACKEND`, …).
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

/// Entrypoint + bindings for a generated native-behind-workerd proxy.
///
/// Does not require an author `[workerd]` module tree. A later host executor
/// writes `module_source` and attaches `PLUGIN_BACKEND`.
#[must_use]
pub fn generated_backend_proxy_plan() -> (EntrypointSource, Vec<BindingSpec>) {
    (
        EntrypointSource::GeneratedBackendProxy {
            module_source: NATIVE_ADAPTER_JS.to_string(),
            entrypoint: "default".into(),
        },
        vec![
            BindingSpec {
                name: "PLUGIN_BACKEND".into(),
                target: BindingTarget::ExternalFetch {
                    address: "unix:native-broker".into(),
                },
            },
            BindingSpec {
                name: "GRANTED".into(),
                target: BindingTarget::IsolateService {
                    service: "granted".into(),
                },
            },
        ],
    )
}

/// Materialize a generated adapter isolate with `PLUGIN_BACKEND` (no author `[workerd]` modules).
///
/// Host executor owns the process tree: it launches workerd and the trusted
/// native broker; the broker connects to the verified native guest. Plugin
/// input cannot choose the executable or weaken the sandbox.
///
/// `state_dir` is an existing session directory from [`workerd_state_dir`], or
/// `None` to allocate a unique directory.
///
/// # Errors
///
/// Returns an error when state-dir I/O or config write fails.
#[allow(clippy::too_many_arguments)]
pub fn materialize_native_backend(
    root: &Path,
    egress: &EgressProxy,
    limits: EffectiveWorkerdLimits,
    listen: ListenSpec,
    notify_addr: Option<&str>,
    granted_addr: Option<&str>,
    backend_addr: &str,
    bridge_token: &str,
    state_dir: Option<&Path>,
) -> Result<GeneratedConfig> {
    let state_dir = resolve_state_dir(root, state_dir)?;
    let bookclerk_dir = state_dir.join(".bookclerk");
    fs::create_dir_all(&bookclerk_dir)
        .with_context(|| format!("create {}", bookclerk_dir.display()))?;
    fs::write(bookclerk_dir.join("bridge.js"), BRIDGE_JS)?;
    fs::write(bookclerk_dir.join("egress.js"), EGRESS_JS)?;
    fs::write(bookclerk_dir.join("host_stub.js"), HOST_STUB_JS)?;
    fs::write(bookclerk_dir.join("adapter.js"), NATIVE_ADAPTER_JS)?;
    fs::write(bookclerk_dir.join("sdk-workerd.js"), SDK_WORKERD_JS)?;

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
    extra_services.push_str(&format!(
        r#"    (name = "nativeBackend", external = (address = "{}", http = ())),"#,
        escape_capnp(backend_addr)
    ));
    extra_services.push('\n');

    let host_bindings = match notify_addr {
        Some(addr) => {
            extra_services.push_str(&format!(
                r#"    (name = "hostNotify", external = (address = "{}", http = ())),"#,
                escape_capnp(addr)
            ));
            extra_services.push('\n');
            format!("{bridge_token_binding},\n    (name = \"NOTIFY\", service = \"hostNotify\")")
        }
        None => bridge_token_binding.clone(),
    };

    let mut adapter_bindings = format!(
        r#"{bridge_token_binding},
    (name = "PLUGIN_BACKEND", service = "nativeBackend")"#
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
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "host", worker = .hostWorker),
    (name = "egress", worker = .egressWorker),
    (name = "adapter", worker = .adapterWorker),
    (name = "bridge", worker = .bridgeWorker),
{extra_services}
  ],
  sockets = [
    {socket_line}
  ]
);

const hostWorker :Workerd.Worker = (
  modules = [
    (name = "host_stub.js", esModule = embed ".bookclerk/host_stub.js")
  ],
  compatibilityDate = "{compat_date}",
  bindings = [
    {host_bindings}
  ],
  globalOutbound = "blocked",
);

const egressWorker :Workerd.Worker = (
  modules = [
    (name = "egress.js", esModule = embed ".bookclerk/egress.js")
  ],
  compatibilityDate = "{compat_date}",
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
        host_bindings = host_bindings,
        adapter_bindings = adapter_bindings,
        bridge_bindings = bridge_bindings,
    );

    let config_path = state_dir.join("workerd-config.capnp");
    write_owner_only_file(&config_path, config)?;
    let import_path = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
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
    /// Listen address for the notify / bridge socket.
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
/// - unix notify socket (created by the launcher, not this function)
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
/// `notify_addr` is an optional workerd `external` address (`host:port` or
/// `unix:/path`) for `HOST.notify` → launcher reverse channel.
///
/// `bridge_token` is a per-isolate bearer shared by the launcher, bridge `/rpc`
/// + `/health`, and `HOST.notify` reverse channel (`BRIDGE_TOKEN` binding).
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `manifest` - Parsed plugin manifest.
/// * `egress` - `egress` input for this call.
/// * `listen` - Daemon listen address (`host:port` or URL).
/// * `notify_addr` - String `notify_addr` for this call.
/// * `bridge_token` - Bearer token for isolate → host notify.
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
    notify_addr: Option<&str>,
    granted_addr: Option<&str>,
    bridge_token: &str,
    state_dir: Option<&Path>,
) -> Result<GeneratedConfig> {
    let workerd = manifest
        .workerd
        .as_ref()
        .context("missing [workerd] table")?;

    let state_dir = resolve_state_dir(root, state_dir)?;
    let bookclerk_dir = state_dir.join(".bookclerk");
    fs::create_dir_all(&bookclerk_dir)
        .with_context(|| format!("create {}", bookclerk_dir.display()))?;
    fs::write(bookclerk_dir.join("bridge.js"), BRIDGE_JS)?;
    fs::write(bookclerk_dir.join("egress.js"), EGRESS_JS)?;
    fs::write(bookclerk_dir.join("host_stub.js"), HOST_STUB_JS)?;
    fs::write(bookclerk_dir.join("adapter.js"), ADAPTER_JS)?;

    let modules_dir = root.join(&workerd.modules_dir);
    if !modules_dir.is_dir() {
        bail!("modules dir missing: {}", modules_dir.display());
    }
    let main_rel = format!("{}/{}", workerd.modules_dir, workerd.main_module);
    let main_abs = root.join(&main_rel);
    if !main_abs.is_file() {
        bail!("main module missing: {}", main_abs.display());
    }

    let mut module_files = collect_modules(&modules_dir)?;
    // Ensure main module is first (workerd treats the first module as the entry).
    module_files.retain(|p| p != &main_abs);
    let mut ordered = vec![main_abs.clone()];
    ordered.extend(module_files);

    let modules_prefix = workerd.modules_dir.trim_matches('/').replace('\\', "/");
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
        fs::write(bookclerk_dir.join("sdk-workerd.js"), SDK_WORKERD_JS)?;
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
        fs::write(bookclerk_dir.join("sdk-workerd.py"), SDK_WORKERD_PY)?;
        fs::write(bookclerk_dir.join("sdk-init.py"), SDK_PY_INIT)?;
        if !seen_names.contains(SDK_PY_INIT_MODULE) {
            module_embeds.push(format!(
                r#"(name = "{}", pythonModule = embed ".bookclerk/sdk-init.py")"#,
                escape_capnp(SDK_PY_INIT_MODULE)
            ));
            seen_names.insert(SDK_PY_INIT_MODULE.to_string());
        }
        if !seen_names.contains(SDK_PY_WORKERD_MODULE) {
            module_embeds.push(format!(
                r#"(name = "{}", pythonModule = embed ".bookclerk/sdk-workerd.py")"#,
                escape_capnp(SDK_PY_WORKERD_MODULE)
            ));
            seen_names.insert(SDK_PY_WORKERD_MODULE.to_string());
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

    // Host/egress/bridge stay plain JS — never inherit python_workers (heavy).
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
            r#"(name = "PLUGIN", service = "plugin", entrypoint = "{}")"#,
            escape_capnp(entrypoint)
        )
    };
    let adapter_service_binding = r#"(name = "PLUGIN", service = "adapter")"#;

    let socket_line = listen.workerd_socket_line();
    // Never force unrestricted `internet` for Python under Deny. Outbound +
    // Python uses the egress proxy with Pyodide CDN hosts auto-allowlisted.
    let plugin_outbound = plugin_global_outbound(egress.mode());

    let bridge_token_binding = format!(
        r#"(name = "BRIDGE_TOKEN", text = "{}")"#,
        escape_capnp(bridge_token)
    );

    let mut extra_services = String::new();
    let host_bindings = match notify_addr {
        Some(addr) => {
            extra_services.push_str(&format!(
                r#"    (name = "hostNotify", external = (address = "{}", http = ())),"#,
                escape_capnp(addr)
            ));
            extra_services.push('\n');
            format!("{bridge_token_binding},\n    (name = \"NOTIFY\", service = \"hostNotify\")")
        }
        None => bridge_token_binding.clone(),
    };
    // Bridge talks to the adapter isolate. GRANTED / BRIDGE_TOKEN are adapter-private
    // (not author `pluginWorker` bindings). wrapV2Plugin stripping keys is hygiene.
    let bridge_bindings = format!("{adapter_service_binding},\n    {bridge_token_binding}");
    let mut adapter_bindings = format!("{plugin_service_binding},\n    {bridge_token_binding}");
    let plugin_bindings = String::from(r#"(name = "HOST", service = "host")"#);
    if let Some(addr) = granted_addr {
        extra_services.push_str(&format!(
            r#"    (name = "granted", external = (address = "{}", http = ())),"#,
            escape_capnp(addr)
        ));
        extra_services.push('\n');
        adapter_bindings.push_str(",\n    (name = \"GRANTED\", service = \"granted\")");
    }

    // Adapter always loads the JS SDK (even when the author isolate is Python).
    fs::write(bookclerk_dir.join("sdk-workerd.js"), SDK_WORKERD_JS)?;
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
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "host", worker = .hostWorker),
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

const hostWorker :Workerd.Worker = (
  modules = [
    (name = "host_stub.js", esModule = embed ".bookclerk/host_stub.js")
  ],
  compatibilityDate = "{compat_date}",
  {bridge_flags}
  bindings = [
    {host_bindings}
  ],
  globalOutbound = "blocked",
);

const egressWorker :Workerd.Worker = (
  modules = [
    (name = "egress.js", esModule = embed ".bookclerk/egress.js")
  ],
  compatibilityDate = "{compat_date}",
  {bridge_flags}
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
  bindings = [
    {plugin_bindings}
  ],
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
        host_bindings = host_bindings,
        adapter_bindings = adapter_bindings,
        bridge_bindings = bridge_bindings,
        plugin_bindings = plugin_bindings,
        plugin_outbound = plugin_outbound,
    );

    let config_path = state_dir.join("workerd-config.capnp");
    write_owner_only_file(&config_path, config)?;

    let import_path = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

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
fn collect_modules_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_modules_inner(&path, out)?;
            continue;
        }
        if !path.is_file() {
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
        use bookclerk_plugin_manifest::{
            CapabilitiesManifest, NetworkCapabilities, PluginKind, PluginManifest,
            PluginRuntimeKind, WorkerdLimits, WorkerdRuntimeManifest,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("modules");
        std::fs::create_dir_all(&modules).expect("modules dir");
        std::fs::write(modules.join("index.js"), "export default {};").expect("index.js");
        let manifest = PluginManifest {
            api_version: 2,
            id: "echo".into(),
            name: None,
            kind: PluginKind::Integration,
            version: None,
            logo: None,
            runtime: PluginRuntimeKind::Workerd,
            command: None,
            args: vec![],
            workerd: Some(WorkerdRuntimeManifest {
                compatibility_date: "2026-08-01".into(),
                compatibility_flags: flags.iter().map(|s| (*s).to_string()).collect(),
                main_module: "index.js".into(),
                modules_dir: "modules".into(),
                entrypoint: "default".into(),
                limits: WorkerdLimits::default(),
            }),
            modules: vec![],
            capabilities: CapabilitiesManifest {
                network: NetworkCapabilities {
                    mode,
                    domains: if mode == NetworkMode::Outbound {
                        vec!["example.com".into()]
                    } else {
                        vec![]
                    },
                },
                bindings: Default::default(),
                methods: Default::default(),
            },
            cli: None,
            oidc: Default::default(),
        };
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
            None,
            "test-bridge-token",
            None,
        )
        .expect("materialize");
        std::fs::read_to_string(&generated.config_path).expect("read capnp")
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
                capnp.contains("const hostWorker")
                    && capnp.contains("globalOutbound = \"blocked\""),
                "host worker must stay blocked: {flags:?}"
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
        use bookclerk_plugin_manifest::{
            CapabilitiesManifest, NetworkCapabilities, NetworkMode, PluginKind, PluginManifest,
            PluginRuntimeKind, WorkerdLimits, WorkerdRuntimeManifest,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("modules");
        std::fs::create_dir_all(&modules).expect("modules dir");
        std::fs::write(modules.join("index.js"), "export default {};").expect("index.js");
        let manifest = PluginManifest {
            api_version: 2,
            id: "v2_stream".into(),
            name: None,
            kind: PluginKind::Output,
            version: None,
            logo: None,
            runtime: PluginRuntimeKind::Workerd,
            command: None,
            args: vec![],
            workerd: Some(WorkerdRuntimeManifest {
                compatibility_date: "2026-08-01".into(),
                compatibility_flags: vec![],
                main_module: "index.js".into(),
                modules_dir: "modules".into(),
                entrypoint: "default".into(),
                limits: WorkerdLimits::default(),
            }),
            modules: vec![],
            capabilities: CapabilitiesManifest {
                network: NetworkCapabilities {
                    mode: NetworkMode::Deny,
                    domains: vec![],
                },
                bindings: Default::default(),
                methods: Default::default(),
            },
            cli: None,
            oidc: Default::default(),
        };
        let generated = materialize(
            dir.path(),
            &manifest,
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            Some("unix:/tmp/granted.sock"),
            "test-bridge-token",
            None,
        )
        .expect("materialize");
        let capnp = std::fs::read_to_string(&generated.config_path).expect("read capnp");
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
            name: "PLUGIN_BACKEND".into(),
            target: BindingTarget::IsolateService {
                service: "backend".into(),
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
                assert!(module_source.contains("wrapV2PluginFromNative"));
                assert!(!module_source.contains("wrapV2PluginFromBinding"));
                assert_eq!(entrypoint, "default");
            }
            EntrypointSource::AuthorModules { .. } => panic!("expected generated proxy"),
        }
        assert!(bindings.iter().any(|b| b.name == "GRANTED"));
        assert!(bindings.iter().any(|b| b.name == "PLUGIN_BACKEND"));
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
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            Some("unix:/tmp/granted.sock"),
            "127.0.0.1:9",
            "token",
            None,
        )
        .expect("materialize native");
        let capnp = std::fs::read_to_string(&generated.config_path).expect("read");
        assert!(capnp.contains(r#"(name = "PLUGIN_BACKEND", service = "nativeBackend")"#));
        assert!(capnp.contains(r#"(name = "nativeBackend", external"#));
        assert!(
            !capnp.contains("const pluginWorker"),
            "native-behind-workerd must not require an author plugin isolate:\n{capnp}"
        );
        assert!(!dir.path().join("modules").is_dir());
    }

    #[test]
    fn workerd_state_dir_unix_sockets_fit_sockaddr_un() {
        let scratch =
            PathBuf::from("/home/runner/work/_temp/BookclerkFiles/plugins/echo_native_node/tmp");
        let dir = scratch.join(workerd_state_leaf(
            &scratch.join("plugin-root"),
            &"f".repeat(SESSION_NONCE_HEX),
        ));
        let granted = dir.join("granted.sock");
        assert!(
            granted.to_string_lossy().len() < 108,
            "granted socket path too long for sockaddr_un: {} ({} bytes)",
            granted.display(),
            granted.to_string_lossy().len()
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
                        &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
                        bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
                        ListenSpec::InheritedTcp { port: 9 },
                        None,
                        Some("unix:/tmp/granted.sock"),
                        "127.0.0.1:9",
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
            let _ = std::fs::remove_dir_all(dir);
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
                            &EgressProxy::from_policy(
                                bookclerk_plugin_manifest::EgressPolicy::deny(),
                            ),
                            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
                            ListenSpec::InheritedTcp { port: 9 },
                            None,
                            Some("unix:/tmp/granted.sock"),
                            "127.0.0.1:9",
                            "token",
                            None,
                        )
                        .expect("materialize native");
                        let capnp = std::fs::read_to_string(&generated.config_path).expect("read");
                        assert!(
                            capnp.contains(
                                r#"(name = "PLUGIN_BACKEND", service = "nativeBackend")"#
                            ),
                            "native config clobbered:\n{capnp}"
                        );
                        let _ = std::fs::remove_dir_all(&generated.state_dir);
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
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            Some("unix:/tmp/granted.sock"),
            "127.0.0.1:9",
            "token",
            None,
        )
        .expect("materialize");
        let dir_mode = std::fs::metadata(&generated.state_dir)
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
        let cfg_mode = std::fs::metadata(&generated.config_path)
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
        let _ = std::fs::remove_dir_all(&generated.state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_state_dir_chmods_existing_session_dir() {
        use std::os::unix::fs::PermissionsExt;

        let plugin = tempfile::tempdir().expect("plugin");
        let session = tempfile::tempdir().expect("session");
        std::fs::set_permissions(session.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        let generated = materialize_native_backend(
            plugin.path(),
            &EgressProxy::from_policy(bookclerk_plugin_manifest::EgressPolicy::deny()),
            bookclerk_plugin_manifest::WorkerdLimits::default().effective(),
            ListenSpec::InheritedTcp { port: 9 },
            None,
            Some("unix:/tmp/granted.sock"),
            "127.0.0.1:9",
            "token",
            Some(session.path()),
        )
        .expect("materialize");
        let dir_mode = std::fs::metadata(session.path())
            .expect("dir meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "existing session dir must be tightened to 0700"
        );
        let cfg_mode = std::fs::metadata(&generated.config_path)
            .expect("cfg meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(cfg_mode, 0o600);
    }
}
