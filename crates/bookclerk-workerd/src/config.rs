//! Generate a real workerd Cap'n Proto config for one plugin isolate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::{
    manifest_needs_python, with_python_runtime_hosts, NetworkMode, PluginManifest,
};

use crate::egress::EgressProxy;

const BRIDGE_JS: &str = include_str!("../bridge/bridge.js");
const EGRESS_JS: &str = include_str!("../bridge/egress.js");
const HOST_STUB_JS: &str = include_str!("../bridge/host_stub.js");
/// Injected as `@bookclerk/plugin-sdk` + `@bookclerk/plugin-sdk/workerd`.
const SDK_WORKERD_JS: &str = include_str!("../../../packages/plugin-sdk/embed/bookclerk_plugin.js");
/// Injected as `bookclerk_plugin_sdk/workerd.py`.
const SDK_WORKERD_PY: &str =
    include_str!("../../../packages/plugin-sdk-python/src/bookclerk_plugin_sdk/workerd.py");
const SDK_PY_INIT: &str = concat!(
    "\"\"\"Bookclerk plugin SDK (workerd isolate).\n\n",
    "Use: from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js\n\n",
    "Native stdio guests use the pip package's BookclerkPlugin +\n",
    "BookclerkPluginGuest.serve instead.\n",
    "\"\"\"\n"
);

/// Package import names authors use for the workerd BookclerkPlugin.
pub const SDK_JS_MODULE_NAMES: &[&str] =
    &["@bookclerk/plugin-sdk/workerd", "@bookclerk/plugin-sdk"];
/// Python package path for `from bookclerk_plugin_sdk.workerd import …`.
pub const SDK_PY_WORKERD_MODULE: &str = "bookclerk_plugin_sdk/workerd.py";
pub const SDK_PY_INIT_MODULE: &str = "bookclerk_plugin_sdk/__init__.py";

// Re-export so call sites / docs can discover the shared list beside workerd config.
pub use bookclerk_plugin_manifest::PYODIDE_EGRESS_HOSTS;

/// Where bridge assets + Cap'n Proto config are written.
///
/// Prefer `$TMPDIR` (the guest scratch dir inside a jail). The plugin install
/// root is read-only under Landlock, so materializing `.bookclerk/` there fails.
pub fn workerd_state_dir(plugin_root: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("TMPDIR")
        .or_else(|| std::env::var_os("TEMP"))
        .or_else(|| std::env::var_os("TMP"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| plugin_root.join(".bookclerk-state"));
    let dir = base.join("bookclerk-workerd");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

/// How the bridge HTTP socket is exposed to `bookclerk-workerd`.
#[derive(Debug, Clone)]
pub enum ListenSpec {
    /// `127.0.0.1:port` — workerd binds itself (unconfined smoke / Windows).
    TcpLoopback(u16),
    /// Parent already bound `127.0.0.1:0`; workerd inherits via `--socket-fd`.
    /// Required under Linux Landlock `OutboundListen` (only `bind(port=0)` is
    /// allowed — rebinding a concrete ephemeral port is EPERM).
    InheritedTcp { port: u16 },
}

impl ListenSpec {
    /// Cap'n Proto `sockets` entry for the bridge RPC listener.
    ///
    /// Inherited FDs omit `address` — workerd gets `--socket-fd=rpc=<fd>`.
    #[must_use]
    pub fn workerd_socket_line(&self) -> String {
        match self {
            Self::TcpLoopback(port) => format!(
                r#"(name = "rpc", address = "127.0.0.1:{}", http = (), service = "bridge")"#,
                port
            ),
            Self::InheritedTcp { .. } => {
                r#"(name = "rpc", http = (), service = "bridge")"#.to_string()
            }
        }
    }

    #[must_use]
    pub fn client_base_url(&self) -> String {
        match self {
            Self::TcpLoopback(port) | Self::InheritedTcp { port } => {
                format!("http://127.0.0.1:{port}")
            }
        }
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        match self {
            Self::TcpLoopback(port) | Self::InheritedTcp { port } => *port,
        }
    }
}

/// Materialize bridge assets + config under `root`, return config path and socket addr hint.
pub struct GeneratedConfig {
    pub config_path: PathBuf,
    pub listen: ListenSpec,
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
/// `notify_addr` is an optional workerd `external` address (`host:port` or
/// `unix:/path`) for `HOST.notify` → launcher reverse channel.
pub fn materialize(
    root: &Path,
    manifest: &PluginManifest,
    egress: &EgressProxy,
    listen: ListenSpec,
    notify_addr: Option<&str>,
) -> Result<GeneratedConfig> {
    let workerd = manifest
        .workerd
        .as_ref()
        .context("missing [workerd] table")?;

    let state_dir = workerd_state_dir(root)?;
    let bookclerk_dir = state_dir.join(".bookclerk");
    fs::create_dir_all(&bookclerk_dir)
        .with_context(|| format!("create {}", bookclerk_dir.display()))?;
    fs::write(bookclerk_dir.join("bridge.js"), BRIDGE_JS)?;
    fs::write(bookclerk_dir.join("egress.js"), EGRESS_JS)?;
    fs::write(bookclerk_dir.join("host_stub.js"), HOST_STUB_JS)?;

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
    let mut policy = egress.policy().clone();
    policy.domains = domains;
    let policy_json = policy.to_policy_json();
    let policy_escaped = escape_capnp(&policy_json.to_string());

    let entrypoint = workerd.entrypoint.as_str();
    let entrypoint_binding = if entrypoint == "default" {
        r#"(name = "PLUGIN", service = "plugin")"#.to_string()
    } else {
        format!(
            r#"(name = "PLUGIN", service = "plugin", entrypoint = "{}")"#,
            escape_capnp(entrypoint)
        )
    };

    let socket_line = listen.workerd_socket_line();
    // Never force unrestricted `internet` for Python under Deny. Outbound +
    // Python uses the egress proxy with Pyodide CDN hosts auto-allowlisted.
    let plugin_outbound = plugin_global_outbound(egress.mode());

    let (notify_service, host_bindings) = match notify_addr {
        Some(addr) => (
            format!(
                r#"    (name = "hostNotify", external = (address = "{}", http = ())),"#,
                escape_capnp(addr)
            ),
            r#"(name = "NOTIFY", service = "hostNotify")"#.to_string(),
        ),
        None => (String::new(), String::new()),
    };

    let config = format!(
        r#"using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "host", worker = .hostWorker),
    (name = "egress", worker = .egressWorker),
    (name = "plugin", worker = .pluginWorker),
    (name = "bridge", worker = .bridgeWorker),
{notify_service}
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
    (name = "HOST", service = "host"),
  ],
  globalOutbound = "{plugin_outbound}",
);

const bridgeWorker :Workerd.Worker = (
  modules = [
    (name = "bridge.js", esModule = embed ".bookclerk/bridge.js")
  ],
  compatibilityDate = "{compat_date}",
  {bridge_flags}
  bindings = [
    {entrypoint_binding}
  ],
  globalOutbound = "blocked",
);
"#,
        socket_line = socket_line,
        compat_date = escape_capnp(&workerd.compatibility_date),
        bridge_flags = bridge_flags,
        plugin_flags = flags_line,
        modules = module_embeds.join(",\n    "),
        policy_escaped = policy_escaped,
        entrypoint_binding = entrypoint_binding,
        plugin_outbound = plugin_outbound,
        notify_service = notify_service,
        host_bindings = host_bindings,
    );

    let config_path = state_dir.join("workerd-config.capnp");
    fs::write(&config_path, config).with_context(|| format!("write {}", config_path.display()))?;

    let import_path = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

    Ok(GeneratedConfig {
        config_path,
        listen,
        state_dir,
        import_path,
    })
}

/// Plugin isolate `globalOutbound`: Deny → blocked; Outbound → egress proxy.
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
#[must_use]
pub fn egress_domains_for(needs_python: bool, mode: NetworkMode, base: &[String]) -> Vec<String> {
    with_python_runtime_hosts(needs_python, mode, base)
}

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

fn collect_modules(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_modules_inner(dir, &mut out)?;
    out.sort();
    Ok(out)
}

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
}
