"""Materialize Cap'n Proto workerd config + bridge (mirrors config.rs).

Writes ``.bookclerk/`` bridge + adapter assets and a Cap'n Proto config that embeds plugin
modules plus the injected Python/JS SDK. Used by the sparse smoke launcher and
authoring tools that do not ship the Rust ``bookclerk-workerd`` binary.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
from pathlib import Path
from typing import Any, NamedTuple

from .ensure import package_root
from ..path_guard import (
    resolve_under,
    refuse_symlink_path,
    ensure_dir_under,
    write_file_under,
    copy_file_under,
)

SDK_JS_MODULE_NAMES = ("@bookclerk/plugin-sdk/workerd", "@bookclerk/plugin-sdk")
"""Module names used when embedding the TypeScript workerd SDK."""
SDK_PY_WORKERD_MODULE = "bookclerk_plugin_sdk/workerd.py"
"""Isolate module path for the Python workerd ``BookclerkEntrypoint`` base."""
SDK_PY_INIT_MODULE = "bookclerk_plugin_sdk/__init__.py"
"""Isolate module path for the package ``__init__`` stub."""
PYODIDE_EGRESS_HOSTS = ("cdn.jsdelivr.net", "pypi.org", "files.pythonhosted.org")
"""Extra egress hosts allowed for Python Workers / Pyodide package fetch."""

SDK_PY_INIT = '''"""Bookclerk plugin SDK (workerd isolate).

Use: from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, js

Native guests use Rust serve() / PluginWorker instead.
"""
'''
"""Source text embedded as ``bookclerk_plugin_sdk/__init__.py`` in the isolate."""

ADAPTER_JS = '''import { wrapPluginFromBinding } from "@bookclerk/plugin-sdk/workerd";
export default wrapPluginFromBinding();
'''
"""First-party adapter isolate module (mirrors ``ADAPTER_JS`` in ``config.rs``)."""

ENTRYPOINT_SERVICE_BINDINGS: dict[str, tuple[str, str]] = {
    "storefront": ("PLUGIN_STOREFRONT", "Storefront"),
    "storage": ("PLUGIN_STORAGE", "Storage"),
    "databaseAdapter": ("PLUGIN_DATABASE_ADAPTER", "DatabaseAdapter"),
    "remoteLibrary": ("PLUGIN_REMOTE_LIBRARY", "RemoteLibrary"),
    "cli": ("PLUGIN_CLI", "Cli"),
    "oidc": ("PLUGIN_OIDC", "Oidc"),
}
"""``(binding, exported class)`` the adapter receives per manifest entrypoint."""


def manifest_capabilities(m: dict[str, Any]) -> dict[str, Any]:
    """Typed capability declaration a manifest implies (mirrors Rust ``capabilities()``).

    Args:
        m: Validated manifest dict.

    Returns:
        Wire-shaped ``PluginCapabilities`` object.
    """
    bindings: list[str] = []
    if m.get("vars") is not None:
        bindings.append("CONFIG")
    secrets = m.get("secrets")
    if isinstance(secrets, dict):
        bindings.append(str(secrets.get("binding") or "SECRETS"))
    work_fs = m.get("work_fs")
    if isinstance(work_fs, dict):
        bindings.append(str(work_fs.get("binding") or "WORK_FS"))
    oauth = m.get("oauth")
    if isinstance(oauth, dict):
        bindings.append(str(oauth.get("binding") or "OAUTH"))
    for kv in m.get("kv_namespaces") or []:
        bindings.append(str(kv.get("binding") or "KV"))
    events = m.get("events") or {}
    for producer in events.get("producers") or []:
        name = str(producer.get("binding") or "EVENTS")
        if name not in bindings:
            bindings.append(name)
    return {
        "entrypoints": [str(e) for e in (m.get("entrypoints") or [])],
        "consumes": [
            {
                "eventType": str(c.get("type")),
                "schemaVersions": [int(v) for v in (c.get("schema_versions") or [1])],
                "supportsSuspend": bool(c.get("supports_suspend", False)),
            }
            for c in events.get("consumers") or []
        ],
        "produces": [str(p.get("type")) for p in events.get("producers") or []],
        "jobs": [str(j) for j in ((m.get("triggers") or {}).get("jobs") or [])],
        "databases": [str(d.get("binding")) for d in m.get("databases") or []],
        "bindings": bindings,
    }


def manifest_describe_json(m: dict[str, Any]) -> str:
    """``PLUGIN_DESCRIBE`` JSON the adapter answers ``describe()`` from.

    Args:
        m: Validated manifest dict.

    Returns:
        Serialized wire-shaped ``PluginDescribe`` projection.
    """
    cli = m.get("cli") if isinstance(m.get("cli"), dict) else {}
    return json.dumps(
        {
            "apiVersion": int(m.get("api_version", 3)),
            "id": m["id"],
            "displayName": str(m.get("name") or ""),
            "rpcFeatures": ["rpc.scalarLimits", "rpc.streams"],
            "capabilities": manifest_capabilities(m),
            "cli": {"commands": cli.get("commands") or []},
        },
        separators=(",", ":"),
    )


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


def collect_modules(directory: Path, *, plugin_root: Path | None = None) -> list[Path]:
    """Collect embeddable module files under ``directory``.

    Symlinks are refused (matching the Rust workerd walker) so a nested
    ``modules/leak -> /outside`` cannot recurse or embed files outside the
    plugin install tree. The modules root itself is checked against
    ``plugin_root`` (or ``directory`` when omitted) so a symlinked modules
    directory cannot promote an outside tree into the embed set.

    Args:
        directory: Plugin modules root to walk recursively.
        plugin_root: Original trusted plugin root; defaults to ``directory``.

    Returns:
        Sorted list of ``.js``/``.mjs``/``.py``/``.wasm``/``.json`` file paths.

    Raises:
        ValueError: When a directory entry or the modules root is a symlink.
    """
    import os

    trusted = Path(plugin_root) if plugin_root is not None else directory
    refuse_symlink_path(trusted, directory)
    # Lexical root — do not realpath/promote a symlink target.
    root = Path(os.path.abspath(os.path.normpath(os.fspath(directory))))
    out: list[Path] = []

    def walk(d: Path) -> None:
        for entry in sorted(d.iterdir(), key=lambda p: p.name):
            if entry.is_symlink():
                raise ValueError(
                    f"refusing symlink in workerd modules tree: {entry}"
                )
            p = resolve_under(root, entry.relative_to(root))
            if entry.is_dir():
                walk(p)
                continue
            if not entry.is_file():
                continue
            lower = entry.name.lower()
            if lower.endswith((".js", ".mjs", ".py", ".wasm", ".json")):
                out.append(p)

    walk(root)
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
        resolve_under(sdk_root, "bridge", "bookclerk_plugin.js"),
        resolve_under(sdk_root.parents[2], "plugin-sdk", "embed", "bookclerk_plugin.js"),
    ]
    for c in candidates:
        if c.is_file():
            return c
    raise FileNotFoundError(
        "JS SDK embed (bookclerk_plugin.js) not found; use the TypeScript smoke CLI "
        "for .js plugins, or keep packages/plugin-sdk beside this package"
    )


def _relative_manifest_path(value: str, label: str) -> str:
    """Accept a relative manifest path with normal components only.

    ``dist/modules`` and ``nested/main.js`` are valid. ``edition..2.js`` is a
    filename, not a parent component. Absolute, empty, NUL, and ``..``
    components are rejected.
    """
    if not value or "\0" in value:
        raise ValueError(f"{label} is empty or contains NUL")
    normalized = value.replace("\\", "/")
    if normalized.startswith("/") or (len(normalized) >= 2 and normalized[1] == ":"):
        raise ValueError(f"{label} must be a relative path: {value}")
    parts = [part for part in normalized.split("/") if part not in {"", "."}]
    if not parts:
        raise ValueError(f"{label} is empty: {value}")
    if any(part == ".." for part in parts):
        raise ValueError(f"{label} must not contain parent components: {value}")
    return "/".join(parts)


def _single_file_name(value: str, label: str) -> str:
    if (
        not value
        or "\0" in value
        or "/" in value
        or "\\" in value
        or value in {".", ".."}
    ):
        raise ValueError(f"{label} must be a single path component: {value}")
    return value


class GeneratedConfig(NamedTuple):
    """Paths produced by :func:`materialize_config`."""

    config_path: Path
    listen_addr: str
    state_dir: Path
    import_path: Path


def allocate_workerd_state_dir(plugin_root: Path) -> Path:
    """Allocate a unique writable session directory for workerd generated embeds.

    Keys the leaf by a short hash of the plugin root plus a random suffix from
    ``tempfile.mkdtemp`` under the OS temp dir — never mkdir of a raw operator
    ``TMPDIR`` / plugin-root join (local threat-model path sink).

    Args:
        plugin_root: Plugin install root (used only as an opaque id seed).

    Returns:
        Absolute session directory.
    """
    import tempfile

    root_key = hashlib.sha256(os.fspath(Path(plugin_root).resolve()).encode()).hexdigest()[:8]
    dir_path = Path(tempfile.mkdtemp(prefix=f"bc-w{root_key}-"))
    try:
        dir_path.chmod(0o700)
    except OSError:
        pass
    return Path(os.path.abspath(dir_path))


def materialize_config(
    plugin_root: Path,
    manifest: dict[str, Any],
    *,
    listen_port: int,
    bridge_token: str,
    sdk_root: Path | None = None,
    state_dir: Path | None = None,
    config_name: str = "workerd-config.capnp",
) -> GeneratedConfig:
    """Write bridge assets and Cap'n Proto under a host session directory.

    Generated embeds (``.bookclerk/``, Cap'n Proto) are written under
    ``state_dir`` (allocated under ``$TMPDIR`` when omitted). Author modules
    stay on the read-only install root and are referenced via Cap'n Proto
    ``/modules/…`` embeds resolved with ``workerd serve --import-path``.

    Args:
        plugin_root: Plugin directory that contains ``modules/``.
        manifest: Parsed ``plugin.toml`` mapping (must include ``[workerd]``).
        listen_port: Loopback TCP port for the bridge RPC socket.
        bridge_token: Bearer token required by the bridge HTTP surface.
        sdk_root: SDK package root for bridge/SDK embeds (defaults to this package).
        state_dir: Existing session directory, or ``None`` to allocate one.
        config_name: Output Cap'n Proto filename under the session directory.

    Returns:
        :class:`GeneratedConfig` with config path, listen address, session dir,
        and import path for ``--import-path``.

    Raises:
        ValueError: If ``[workerd]`` is missing or ``bridge_token`` is empty.
        FileNotFoundError: If bridge assets, modules, main module, or SDK embeds
            cannot be found.
    """
    workerd = manifest.get("workerd")
    if not isinstance(workerd, dict):
        raise ValueError("missing [workerd] table")

    plugin_root = Path(plugin_root).resolve()
    sdk_root = Path(sdk_root or package_root()).resolve()
    if state_dir is None:
        state_dir = allocate_workerd_state_dir(plugin_root)
    else:
        state_dir = Path(os.path.realpath(Path(state_dir).resolve()))
    modules_dir_name = _relative_manifest_path(
        str(workerd.get("modules_dir") or "modules"), "modules_dir"
    )
    main_module_name = _relative_manifest_path(str(workerd["main_module"]), "main_module")
    entrypoint = workerd.get("entrypoint") or "default"
    net = (manifest.get("capabilities") or {}).get("network") or {}
    network_mode = net.get("mode") or "deny"
    network_domains = list(net.get("domains") or [])

    bookclerk_dir = ensure_dir_under(state_dir, ".bookclerk")
    refuse_symlink_path(state_dir, bookclerk_dir)
    for name in ("bridge.js", "egress.js"):
        src = resolve_under(sdk_root, "bridge", name)
        if not src.is_file():
            raise FileNotFoundError(f"missing vendored bridge {src}")
        copy_file_under(bookclerk_dir, name, src)
    write_file_under(bookclerk_dir, "adapter.js", ADAPTER_JS)

    modules_dir = resolve_under(plugin_root, modules_dir_name)
    refuse_symlink_path(plugin_root, modules_dir)
    if modules_dir.is_symlink() or not modules_dir.is_dir():
        raise FileNotFoundError(f"modules dir missing: {modules_dir}")
    main_abs = resolve_under(modules_dir, main_module_name)
    refuse_symlink_path(plugin_root, main_abs)
    if main_abs.is_symlink() or not main_abs.is_file():
        raise FileNotFoundError(f"main module missing: {main_abs}")

    module_files = [
        p
        for p in collect_modules(modules_dir, plugin_root=plugin_root)
        if p.resolve() != main_abs.resolve()
    ]
    ordered = [main_abs, *module_files]

    module_embeds: list[str] = []
    needs_python = False
    needs_js = False
    seen_names: set[str] = set()

    for file_path in ordered:
        name = file_path.relative_to(modules_dir).as_posix()
        embed = f"/{modules_dir_name}/{name}"
        if is_legacy_sdk_embed(name):
            continue
        field, python = module_field_for(name)
        if python:
            needs_python = True
        elif name.endswith((".js", ".mjs")):
            needs_js = True
        seen_names.add(name)
        module_embeds.append(
            f'(name = "{escape_capnp(name)}", {field} = embed "{escape_capnp(embed)}")'
        )

    # The adapter isolate always needs the JS SDK embed; the author isolate gets
    # it when it has JS modules.
    sdk_js = _resolve_sdk_js(sdk_root)

    def write_generated(name: str, text: str) -> None:
        write_file_under(bookclerk_dir, name, text)

    write_generated("sdk-workerd.js", sdk_js.read_text(encoding="utf-8"))
    adapter_modules = [
        '(name = "adapter.js", esModule = embed ".bookclerk/adapter.js")',
        *(
            f'(name = "{escape_capnp(mod_name)}", esModule = embed ".bookclerk/sdk-workerd.js")'
            for mod_name in SDK_JS_MODULE_NAMES
        ),
    ]
    if needs_js:
        for mod_name in SDK_JS_MODULE_NAMES:
            if mod_name in seen_names:
                continue
            module_embeds.append(
                f'(name = "{escape_capnp(mod_name)}", esModule = embed ".bookclerk/sdk-workerd.js")'
            )
            seen_names.add(mod_name)

    if needs_python:
        sdk_py = resolve_under(sdk_root, "workerd.py")
        if not sdk_py.is_file():
            raise FileNotFoundError(f"missing Python workerd SDK at {sdk_py}")
        write_generated("sdk-workerd.py", sdk_py.read_text(encoding="utf-8"))
        write_generated("sdk-init.py", SDK_PY_INIT)
        # Modules imported by workerd.py / db_value.py inside the isolate.
        py_siblings = (
            ("bookclerk_plugin_sdk/_abi.py", "_abi.py", "sdk-product-abi.py"),
            ("bookclerk_plugin_sdk/guest_sql.py", "guest_sql.py", "sdk-guest-sql.py"),
            ("bookclerk_plugin_sdk/db_value.py", "db_value.py", "sdk-db-value.py"),
        )
        for mod_name, src_name, embed_file in py_siblings:
            src = resolve_under(sdk_root, src_name)
            if not src.is_file():
                raise FileNotFoundError(f"missing Python workerd SDK module at {src}")
            write_generated(embed_file, src.read_text(encoding="utf-8"))
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
        author_binding = '(name = "PLUGIN", service = "plugin")'
    else:
        author_binding = (
            f'(name = "PLUGIN", service = (name = "plugin", '
            f'entrypoint = "{escape_capnp(entrypoint)}"))'
        )
    named_entrypoint_bindings = [
        f'(name = "{binding}", service = (name = "plugin", entrypoint = "{cls}"))'
        for wire in (manifest.get("entrypoints") or [])
        for binding, cls in [ENTRYPOINT_SERVICE_BINDINGS.get(str(wire), ("", ""))]
        if binding
    ]
    describe_binding = (
        f'(name = "PLUGIN_DESCRIBE", json = "{escape_capnp(manifest_describe_json(manifest))}")'
    )

    listen_addr = f"127.0.0.1:{listen_port}"
    plugin_outbound = plugin_global_outbound(network_mode)
    if not bridge_token:
        raise ValueError("bridge_token is required")
    bridge_token_binding = (
        f'(name = "BRIDGE_TOKEN", text = "{escape_capnp(bridge_token)}")'
    )

    compat_date = escape_capnp(str(workerd["compatibility_date"]))
    modules_joined = ",\n    ".join(module_embeds)
    adapter_modules_joined = ",\n    ".join(adapter_modules)
    adapter_bindings = ",\n    ".join(
        [author_binding, *named_entrypoint_bindings, describe_binding, bridge_token_binding]
    )
    config = f"""using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "egress", worker = .egressWorker),
    (name = "plugin", worker = .pluginWorker),
    (name = "adapter", worker = .adapterWorker),
    (name = "bridge", worker = .bridgeWorker),
  ],
  sockets = [
    (name = "rpc", address = "{listen_addr}", http = (), service = "bridge")
  ]
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
  bindings = [],
  globalOutbound = "{plugin_outbound}",
);

const adapterWorker :Workerd.Worker = (
  modules = [
    {adapter_modules_joined}
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
    (name = "PLUGIN", service = "adapter"),
    {bridge_token_binding}
  ],
  globalOutbound = "blocked",
);
"""

    config_name = _single_file_name(config_name, "config_name")
    config_path = write_file_under(state_dir, config_name, config)
    return GeneratedConfig(
        config_path=config_path,
        listen_addr=listen_addr,
        state_dir=state_dir,
        import_path=plugin_root,
    )
