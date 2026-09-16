"""Materialize Cap'n Proto workerd config + bridge (mirrors config.rs).

Writes ``.bookclerk/`` bridge assets and a Cap'n Proto config that embeds plugin
modules plus the injected Python/JS SDK. Used by the sparse smoke launcher and
authoring tools that do not ship the Rust ``bookclerk-workerd`` binary.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from .ensure import package_root

SDK_JS_MODULE_NAMES = ("@bookclerk/plugin-sdk/workerd", "@bookclerk/plugin-sdk")
"""Module names used when embedding the TypeScript workerd SDK."""
SDK_PY_WORKERD_MODULE = "bookclerk_plugin_sdk/workerd.py"
"""Isolate module path for the Python workerd BookclerkPlugin base."""
SDK_PY_INIT_MODULE = "bookclerk_plugin_sdk/__init__.py"
"""Isolate module path for the package ``__init__`` stub."""
PYODIDE_EGRESS_HOSTS = ("cdn.jsdelivr.net", "pypi.org", "files.pythonhosted.org")
"""Extra egress hosts allowed for Python Workers / Pyodide package fetch."""

SDK_PY_INIT = '''"""Bookclerk plugin SDK (workerd isolate).

Use: from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

Native guests use Rust serve() / PluginRoot instead.
"""
'''
"""Source text embedded as ``bookclerk_plugin_sdk/__init__.py`` in the isolate."""


def escape_capnp(s: str) -> str:
    """Escape a string for embedding inside Cap'n Proto double quotes.

    Args:
        s: Raw string to escape.

    Returns:
        Escaped string safe for Cap'n Proto text literals.
    """
    return s.replace("\\", "\\\\").replace('"', '\\"')


def is_legacy_sdk_embed(name: str) -> bool:
    """Return whether ``name`` is a legacy vendored SDK path to skip.

    Args:
        name: Module path relative to the plugin ``modules/`` directory.

    Returns:
        ``True`` if the file should be omitted in favor of host injection.
    """
    n = name.replace("\\", "/")
    return n in {
        "bookclerk_plugin.js",
        "bookclerk_plugin.py",
        "@bookclerk/plugin-sdk",
        "@bookclerk/plugin-sdk/workerd",
        "@bookclerk/plugin-sdk/workerd.js",
        "bookclerk_plugin_sdk/workerd.py",
        "bookclerk_plugin_sdk/db_value.py",
        "bookclerk_plugin_sdk/_abi.py",
        "bookclerk_plugin_sdk/guest_sql.py",
        "bookclerk_plugin_sdk/__init__.py",
    }


def module_field_for(name: str) -> tuple[str, bool]:
    """Map a module filename to its Cap'n Proto embed field.

    Args:
        name: Module path (used for extension detection).

    Returns:
        A ``(field_name, is_python)`` pair such as ``("pythonModule", True)``.

    Raises:
        ValueError: If the extension is not a supported workerd module type.
    """
    lower = name.lower()
    if lower.endswith(".py"):
        return "pythonModule", True
    if lower.endswith(".wasm"):
        return "wasm", False
    if lower.endswith(".js") or lower.endswith(".mjs"):
        return "esModule", False
    if lower.endswith(".json"):
        return "json", False
    if lower.endswith(".txt") or lower.endswith(".md"):
        return "text", False
    raise ValueError(
        f"unsupported workerd module type for `{name}` (use .js/.mjs/.py/.wasm/.json)"
    )


def collect_modules(directory: Path) -> list[Path]:
    """Collect embeddable module files under ``directory``.

    Args:
        directory: Plugin modules root to walk recursively.

    Returns:
        Sorted list of ``.js``/``.mjs``/``.py``/``.wasm``/``.json`` file paths.
    """
    out: list[Path] = []

    def walk(d: Path) -> None:
        for entry in sorted(d.iterdir(), key=lambda p: p.name):
            if entry.is_dir():
                walk(entry)
                continue
            if not entry.is_file():
                continue
            lower = entry.name.lower()
            if lower.endswith((".js", ".mjs", ".py", ".wasm", ".json")):
                out.append(entry)

    walk(directory)
    out.sort()
    return out


def plugin_global_outbound(mode: str) -> str:
    """Map network mode to the plugin worker ``globalOutbound`` service name.

    Args:
        mode: Manifest network mode (``outbound`` or deny-style).

    Returns:
        ``"egress"`` for outbound mode, otherwise ``"blocked"``.
    """
    return "egress" if mode == "outbound" else "blocked"


def egress_domains_for(needs_python: bool, mode: str, base: list[str]) -> list[str]:
    """Build the egress allow-list, adding Pyodide hosts when needed.

    Args:
        needs_python: Whether the plugin embeds Python modules.
        mode: Network mode from the manifest.
        base: Domains declared in ``capabilities.network.domains``.

    Returns:
        Domain list possibly extended with :data:`PYODIDE_EGRESS_HOSTS`.
    """
    domains = list(base)
    if needs_python and mode == "outbound":
        for host in PYODIDE_EGRESS_HOSTS:
            if not any(d.lower() == host.lower() for d in domains):
                domains.append(host)
    return domains


def _resolve_sdk_js(sdk_root: Path) -> Path:
    candidates = [
        sdk_root.parents[2] / "plugin-sdk" / "embed" / "bookclerk_plugin.js",
    ]
    for c in candidates:
        if c.is_file():
            return c
    raise FileNotFoundError(
        "JS SDK embed (bookclerk_plugin.js) not found; use the TypeScript smoke CLI "
        "for .js plugins, or keep packages/plugin-sdk beside this package"
    )


def materialize_config(
    plugin_root: Path,
    manifest: dict[str, Any],
    *,
    listen_port: int,
    notify_addr: str | None = None,
    bridge_token: str,
    sdk_root: Path | None = None,
    config_name: str = ".bookclerk-workerd-config.capnp",
) -> tuple[Path, str]:
    """Write bridge assets and Cap'n Proto config under ``plugin_root``.

    Args:
        plugin_root: Plugin directory that contains ``modules/`` and receives
            ``.bookclerk/`` plus the config file.
        manifest: Parsed ``plugin.toml`` mapping (must include ``[workerd]``).
        listen_port: Loopback TCP port for the bridge RPC socket.
        notify_addr: Optional host notify address for callback bindings.
        bridge_token: Bearer token required by the bridge HTTP surface.
        sdk_root: SDK package root for bridge/SDK embeds (defaults to this package).
        config_name: Output Cap'n Proto filename under ``plugin_root``.

    Returns:
        ``(config_path, listen_addr)`` where ``listen_addr`` is ``127.0.0.1:<port>``.

    Raises:
        ValueError: If ``[workerd]`` is missing or ``bridge_token`` is empty.
        FileNotFoundError: If bridge assets, modules, main module, or SDK embeds
            cannot be found.
    """
    workerd = manifest.get("workerd")
    if not isinstance(workerd, dict):
        raise ValueError("missing [workerd] table")

    sdk_root = sdk_root or package_root()
    modules_dir_name = workerd.get("modules_dir") or "modules"
    entrypoint = workerd.get("entrypoint") or "default"
    net = (manifest.get("capabilities") or {}).get("network") or {}
    network_mode = net.get("mode") or "deny"
    network_domains = list(net.get("domains") or [])

    bookclerk_dir = plugin_root / ".bookclerk"
    bookclerk_dir.mkdir(parents=True, exist_ok=True)
    bridge_src = sdk_root / "bridge"
    for name in ("bridge.js", "egress.js", "host_stub.js"):
        src = bridge_src / name
        if not src.is_file():
            raise FileNotFoundError(f"missing vendored bridge {src}")
        (bookclerk_dir / name).write_bytes(src.read_bytes())

    modules_dir = plugin_root / modules_dir_name
    if not modules_dir.is_dir():
        raise FileNotFoundError(f"modules dir missing: {modules_dir}")
    main_abs = modules_dir / workerd["main_module"]
    if not main_abs.is_file():
        raise FileNotFoundError(f"main module missing: {main_abs}")

    module_files = [p for p in collect_modules(modules_dir) if p.resolve() != main_abs.resolve()]
    ordered = [main_abs, *module_files]

    module_embeds: list[str] = []
    needs_python = False
    needs_js = False
    seen_names: set[str] = set()

    for file_path in ordered:
        rel = file_path.relative_to(plugin_root).as_posix()
        name = file_path.relative_to(modules_dir).as_posix()
        if is_legacy_sdk_embed(name):
            continue
        field, python = module_field_for(name)
        if python:
            needs_python = True
        elif name.endswith((".js", ".mjs")):
            needs_js = True
        seen_names.add(name)
        module_embeds.append(
            f'(name = "{escape_capnp(name)}", {field} = embed "{escape_capnp(rel)}")'
        )

    if needs_js:
        sdk_js = _resolve_sdk_js(sdk_root)
        (bookclerk_dir / "sdk-workerd.js").write_text(
            sdk_js.read_text(encoding="utf-8"), encoding="utf-8"
        )
        for mod_name in SDK_JS_MODULE_NAMES:
            if mod_name in seen_names:
                continue
            module_embeds.append(
                f'(name = "{escape_capnp(mod_name)}", esModule = embed ".bookclerk/sdk-workerd.js")'
            )
            seen_names.add(mod_name)

    if needs_python:
        sdk_py = sdk_root / "workerd.py"
        if not sdk_py.is_file():
            raise FileNotFoundError(f"missing Python workerd SDK at {sdk_py}")
        (bookclerk_dir / "sdk-workerd.py").write_text(
            sdk_py.read_text(encoding="utf-8"), encoding="utf-8"
        )
        (bookclerk_dir / "sdk-init.py").write_text(SDK_PY_INIT, encoding="utf-8")
        # Modules imported by workerd.py / db_value.py inside the isolate.
        py_siblings = (
            ("bookclerk_plugin_sdk/_abi.py", "_abi.py", "sdk-product-abi.py"),
            ("bookclerk_plugin_sdk/guest_sql.py", "guest_sql.py", "sdk-guest-sql.py"),
            ("bookclerk_plugin_sdk/db_value.py", "db_value.py", "sdk-db-value.py"),
        )
        for mod_name, src_name, embed_file in py_siblings:
            src = sdk_root / src_name
            if not src.is_file():
                raise FileNotFoundError(f"missing Python workerd SDK module at {src}")
            (bookclerk_dir / embed_file).write_text(
                src.read_text(encoding="utf-8"), encoding="utf-8"
            )
            if mod_name not in seen_names:
                module_embeds.append(
                    f'(name = "{escape_capnp(mod_name)}", pythonModule = embed ".bookclerk/{embed_file}")'
                )
                seen_names.add(mod_name)
        if SDK_PY_INIT_MODULE not in seen_names:
            module_embeds.append(
                f'(name = "{escape_capnp(SDK_PY_INIT_MODULE)}", pythonModule = embed ".bookclerk/sdk-init.py")'
            )
            seen_names.add(SDK_PY_INIT_MODULE)
        if SDK_PY_WORKERD_MODULE not in seen_names:
            module_embeds.append(
                f'(name = "{escape_capnp(SDK_PY_WORKERD_MODULE)}", pythonModule = embed ".bookclerk/sdk-workerd.py")'
            )
            seen_names.add(SDK_PY_WORKERD_MODULE)

    flags = [str(f) for f in (workerd.get("compatibility_flags") or [])]
    if needs_python:
        for required in ("python_workers", "disable_python_external_sdk"):
            if required not in flags:
                flags.append(required)
    flags_line = ""
    if flags:
        listed = ", ".join(f'"{escape_capnp(f)}"' for f in flags)
        flags_line = f"compatibilityFlags = [{listed}],"

    domains = egress_domains_for(needs_python, network_mode, network_domains)
    policy_json = json.dumps(
        {
            "mode": "outbound" if network_mode == "outbound" else "deny",
            "domains": domains,
            "maxRedirects": 10,
        },
        separators=(",", ":"),
    )
    policy_escaped = escape_capnp(policy_json)

    if entrypoint == "default":
        entrypoint_binding = '(name = "PLUGIN", service = "plugin")'
    else:
        entrypoint_binding = (
            f'(name = "PLUGIN", service = (name = "plugin", '
            f'entrypoint = "{escape_capnp(entrypoint)}"))'
        )

    listen_addr = f"127.0.0.1:{listen_port}"
    plugin_outbound = plugin_global_outbound(network_mode)
    if not bridge_token:
        raise ValueError("bridge_token is required")
    bridge_token_binding = (
        f'(name = "BRIDGE_TOKEN", text = "{escape_capnp(bridge_token)}")'
    )

    notify_service = ""
    host_bindings = bridge_token_binding
    if notify_addr:
        notify_service = (
            f'    (name = "hostNotify", external = '
            f'(address = "{escape_capnp(notify_addr)}", http = ())),'
        )
        host_bindings = (
            f"{bridge_token_binding},\n    (name = \"NOTIFY\", service = \"hostNotify\")"
        )

    compat_date = escape_capnp(str(workerd["compatibility_date"]))
    modules_joined = ",\n    ".join(module_embeds)
    config = f"""using Workerd = import "/workerd/workerd.capnp";

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
    (name = "rpc", address = "{listen_addr}", http = (), service = "bridge")
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

const pluginWorker :Workerd.Worker = (
  modules = [
    {modules_joined}
  ],
  compatibilityDate = "{compat_date}",
  {flags_line}
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
  
  bindings = [
    {entrypoint_binding},
    {bridge_token_binding}
  ],
  globalOutbound = "blocked",
);
"""

    config_path = plugin_root / config_name
    config_path.write_text(config, encoding="utf-8")
    return config_path, listen_addr
