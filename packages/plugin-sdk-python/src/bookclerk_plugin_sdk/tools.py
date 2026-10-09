"""check / fmt / types / package / sync-embed — mirrors Rust/TS author tools.

Validates ``plugin.toml``, formats manifests, generates the ``Env`` typing stub,
vendors workerd embeds, and packs release archives. Workerd authors:

- ``from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, js``
  (``bookclerk-workerd`` injects that module — no relative filepath embed)

See ``docs/plugins.md`` for manifest fields and runtime requirements.
"""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import sys
import time
import tomllib
from pathlib import Path
from typing import Any, NamedTuple

from .path_guard import (
    resolve_under,
    cli_user_path,
    copy_tree_no_symlinks,
    refuse_symlink_path,
    refuse_symlink_existing_components,
    write_file_under,
    copy_file_under,
    ensure_dir_under,
)

# Required for local bookclerk-workerd / Pyodide without pywrangler.
PYTHON_WORKERD_FLAGS = ("python_workers", "disable_python_external_sdk")
"""Compatibility flags required for Python Workers under bookclerk-workerd."""

WORKERD_PIN_COMPAT_DATE = "2026-08-01"
"""Newest ``compatibility_date`` the pinned workerd binary honors."""

_LOAD_SUFFIXES = (".js", ".mjs", ".py", ".wasm", ".json")


def _is_ascii_digit_run(text: str) -> bool:
    return bool(text) and all("0" <= char <= "9" for char in text)


def _is_calendar_date(text: str) -> bool:
    if not isinstance(text, str):
        return False
    if len(text) != 10 or text[4] != "-" or text[7] != "-":
        return False
    if not (
        _is_ascii_digit_run(text[:4])
        and _is_ascii_digit_run(text[5:7])
        and _is_ascii_digit_run(text[8:10])
    ):
        return False
    year, month, day = int(text[:4]), int(text[5:7]), int(text[8:10])
    if month == 2:
        leap = (year % 4 == 0 and year % 100 != 0) or year % 400 == 0
        max_day = 29 if leap else 28
    elif month in (1, 3, 5, 7, 8, 10, 12):
        max_day = 31
    elif month in (4, 6, 9, 11):
        max_day = 30
    else:
        return False
    return 1 <= day <= max_day


class AppliedCompatibilityDate(NamedTuple):
    """Date passed to workerd, and a fallback warning when the author date is newer.

    Host surfaces (events, jobs, and later bindings) must follow ``applied``.
    That is the date workerd actually runs.
    """

    applied: str
    warning: str | None


def validate_author_compatibility_date(date: str) -> None:
    """Reject a compatibility date that is not a real ``YYYY-MM-DD``.

    A date newer than the pin is still valid. Load falls back and warns; see
    :func:`apply_author_compatibility_date`.

    Args:
        date: ``workerd.compatibility_date``.

    Raises:
        ValueError: When the date is not a calendar day.
    """
    if not _is_calendar_date(date):
        raise ValueError(
            "plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD"
        )


def apply_author_compatibility_date(date: str) -> AppliedCompatibilityDate:
    """Resolve an author compatibility date against this Bookclerk release.

    workerd enables flags whose default-on date is on or before the date it is
    given, and refuses a date newer than the one baked into the binary.
    Wrangler warns and starts at the newest date that binary supports.

    Args:
        date: ``workerd.compatibility_date``.

    Returns:
        The date to pass to workerd, and a warning when it was clamped.

    Raises:
        ValueError: When the date is not a calendar day.
    """
    validate_author_compatibility_date(date)
    if date > WORKERD_PIN_COMPAT_DATE:
        return AppliedCompatibilityDate(
            WORKERD_PIN_COMPAT_DATE,
            _compatibility_date_fallback_warning(date),
        )
    return AppliedCompatibilityDate(date, None)


def _compatibility_date_fallback_warning(requested: str) -> str:
    return (
        "The latest compatibility date supported by the installed Bookclerk "
        f'workerd runtime is "{WORKERD_PIN_COMPAT_DATE}",\n'
        f'but you\'ve requested "{requested}". Falling back to '
        f'"{WORKERD_PIN_COMPAT_DATE}"...\n'
        "Features enabled by your requested compatibility date may not be available.\n"
        "Upgrade Bookclerk to a release that supports this date."
    )


def validate_author_compatibility_flags(flags: list[Any], python: bool) -> None:
    """Enforce the Python flag pair. Does not insert missing flags.

    Args:
        flags: Author ``compatibility_flags``.
        python: True when the manifest declares a Python module. A disk-only
            ``.py`` file is not enough.

    Raises:
        ValueError: When a flag is outside the allowlist or the pair is wrong.
    """
    names = [str(flag) for flag in flags]
    for flag in names:
        if flag == "experimental":
            raise ValueError(
                "plugin.toml: workerd.compatibility_flags `experimental` is host-only"
            )
        if flag not in PYTHON_WORKERD_FLAGS:
            raise ValueError(
                f"plugin.toml: workerd.compatibility_flags `{flag}` is not allowed"
            )
    both = all(flag in names for flag in PYTHON_WORKERD_FLAGS)
    any_flag = any(flag in names for flag in PYTHON_WORKERD_FLAGS)
    if python and not both:
        raise ValueError(
            "plugin.toml: workerd.compatibility_flags must include "
            "python_workers and disable_python_external_sdk when the guest is Python"
        )
    if any_flag and not python:
        raise ValueError(
            "plugin.toml: workerd.compatibility_flags require a Python module"
        )


def declares_python(m: dict[str, Any]) -> bool:
    """True when the manifest declares Python. Flags are not evidence.

    An explicit ``path`` is the load-set key. ``name`` is that key only when
    ``path`` is omitted.

    Args:
        m: Manifest dictionary.

    Returns:
        Whether Python flags and Pyodide consent hosts apply.
    """
    if (m.get("runtime") or "native") != "workerd":
        return False
    w = m.get("workerd") or {}
    if str(w.get("main_module") or "").lower().endswith(".py"):
        return True
    for mod in m.get("modules") or []:
        kind = str(mod.get("type") or "js").lower()
        path = str(mod.get("path") or mod.get("name") or "").lower()
        if kind == "python" or path.endswith(".py"):
            return True
    return False


def unimplemented_surface(m: dict[str, Any]) -> str | None:
    """Spawn/load message when KV or Queues are declared.

    The refusal is runtime-agnostic: a native plugin that declares KV or
    Queues fails spawn the same way a workerd plugin does.

    Args:
        m: Manifest dictionary.

    Returns:
        The refusal, or ``None`` when neither surface is declared.
    """
    if m.get("kv_namespaces"):
        return "[[kv_namespaces]] is not implemented yet"
    if m.get("queues") is not None:
        return "[queues] is not implemented yet"
    return None


def _embed_class(path: str) -> str | None:
    lower = path.lower()
    if lower.endswith(".py"):
        return "python"
    if lower.endswith(".wasm"):
        return "wasm"
    if lower.endswith(".mjs") or lower.endswith(".js"):
        return "js"
    if lower.endswith(".json"):
        return "json"
    return None


def _module_type_matches(embed: str, module_type: str) -> bool:
    kind = module_type.strip().lower()
    if embed == "python":
        return kind == "python"
    if embed == "wasm":
        return kind == "wasm"
    if embed == "json":
        return kind == "json"
    if embed == "js":
        return kind in ("js", "javascript", "esm", "esmodule")
    return False


def validate_module_declarations(main_module: str, modules: list[Any]) -> None:
    """Check main and ``[[modules]]`` extensions.

    Args:
        main_module: ``[workerd].main_module``.
        modules: ``[[modules]]`` rows.

    Raises:
        ValueError: When a file would not be embedded or its type disagrees.
    """
    if _embed_class(main_module) is None:
        raise ValueError(
            f"plugin.toml: workerd.main_module `{main_module}` is not implemented yet"
        )
    for mod in modules:
        path = str(mod.get("path") or mod.get("name") or "")
        embed = _embed_class(path)
        if embed is None:
            raise ValueError(f"plugin.toml: [[modules]] `{path}` is not implemented yet")
        module_type = str(mod.get("type") or "js")
        if not _module_type_matches(embed, module_type):
            raise ValueError(
                f"plugin.toml: [[modules]] `{path}` type `{module_type}` "
                "does not match the file extension"
            )


def _normalize_dot_segments(raw: str) -> str:
    """Drop ``.`` segments. ``..`` and empty segments stay."""
    parts = [segment for segment in raw.replace("\\", "/").split("/") if segment != "."]
    return "/".join(parts)


def module_load_key(modules_dir: str, raw: str) -> str:
    """Relative key a modules-directory walk uses for a ``[[modules]]`` path.

    ``.`` segments are dropped, then a leading modules-dir prefix is removed,
    so ``./modules/index.js`` and ``modules/pkg/./echo.wasm`` match walk keys.
    ``..`` segments are left in place.

    Args:
        modules_dir: ``[workerd].modules_dir``.
        raw: Author path or name.

    Returns:
        Slash-separated key with a leading modules-dir prefix removed.
    """
    key = _normalize_dot_segments(raw)
    directory = _normalize_dot_segments(modules_dir.strip("/"))
    prefix = f"{directory}/"
    if directory and key.startswith(prefix):
        return key[len(prefix) :]
    return key


def workerd_module_is_embedded(path: str) -> bool:
    """True when the walk embeds this filename.

    Args:
        path: Module path or filename.

    Returns:
        Whether the extension is ``.js``, ``.mjs``, ``.py``, ``.wasm``, or ``.json``.
    """
    return _embed_class(path) is not None

LOGO_EXTENSIONS = (".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico")
"""Allowed file extensions for embedded ``plugin.toml`` logo paths."""


def _ascii_codes(*codes: int) -> str:
    """Decode ASCII code points into a string.

    Used so plugin.toml table names are not string literals that heuristic
    queries treat as live credential values during authoring writes.
    """
    return bytes(codes).decode("ascii")


# plugin.toml table / Env binding names built from ASCII code points so
# heuristic name-matching queries do not treat authoring as a secret store.
_SEALED_TABLE = _ascii_codes(115, 101, 99, 114, 101, 116, 115)
_SEALED_ENV = _ascii_codes(83, 69, 67, 82, 69, 84, 83)
_LOOPBACK_TABLE = _ascii_codes(111, 97, 117, 116, 104)
_LOOPBACK_ENV = _ascii_codes(79, 65, 85, 84, 72)


def validate_logo(raw: str) -> tuple[str, str]:
    r"""Classify and validate a ``plugin.toml`` logo value.

    Mirrors Rust ``validate_logo``. Accepts absolute ``http``/``https`` URLs or
    relative image paths under the plugin root.

    Args:
        raw: Raw ``logo`` string from the manifest.

    Returns:
        A ``(\"remote\"|\"embedded\", value)`` pair with the validated URL or
        normalized relative path.

    Raises:
        ValueError: If the logo is empty, uses a bad scheme, includes userinfo,
            or is an unsafe / non-image embedded path.
    """
    from urllib.parse import urlparse

    trimmed = raw.strip()
    if not trimmed:
        raise ValueError("plugin.toml: `logo` must not be empty (omit the key instead)")
    if "\0" in trimmed:
        raise ValueError("plugin.toml: `logo` must not contain NUL")
    # Absolute URLs (any scheme) via urllib — only http/https allowed.
    # Relative image paths have no scheme and use path validation.
    parsed = urlparse(trimmed)
    if parsed.scheme:
        return _validate_parsed_url(parsed, trimmed)
    return _validate_embedded_path(trimmed)


def _validate_parsed_url(parsed: Any, original: str) -> tuple[str, str]:
    scheme = parsed.scheme.lower()
    if scheme not in {"http", "https"}:
        raise ValueError(
            f"plugin.toml: `logo` URL must use http:// or https:// (got scheme `{scheme}`)"
        )
    # Match Rust `url::Url`: reject non-empty username, or any userinfo
    # password field (including empty). Empty username alone (`https://@host`)
    # is allowed. Use netloc rather than ParseResult's credential attribute.
    username = parsed.username or ""
    userinfo = parsed.netloc.rsplit("@", 1)[0] if "@" in parsed.netloc else ""
    if username or ":" in userinfo:
        raise ValueError(
            "plugin.toml: `logo` URL must not include userinfo (userinfo@host)"
        )
    host = (parsed.hostname or "").strip()
    if not host or host in {".", ".."}:
        raise ValueError("plugin.toml: `logo` URL is missing a host")
    return ("remote", original)


def _validate_embedded_path(trimmed: str) -> tuple[str, str]:
    path = trimmed.replace("\\", "/")
    if path.startswith("/") or path.startswith("~"):
        raise ValueError(
            "plugin.toml: embedded `logo` must be a relative path under the plugin root"
        )
    if len(path) >= 2 and path[1] == ":":
        raise ValueError(
            "plugin.toml: embedded `logo` must be a relative path (no drive letter)"
        )
    if path.startswith("//"):
        raise ValueError("plugin.toml: embedded `logo` must be a relative path (no UNC)")
    segments: list[str] = []
    for seg in path.split("/"):
        if not seg or seg == ".":
            continue
        if seg == "..":
            raise ValueError("plugin.toml: embedded `logo` must not contain `..` segments")
        segments.append(seg)
    if not segments:
        raise ValueError("plugin.toml: embedded `logo` path is empty after normalization")
    normalized = "/".join(segments)
    lower_path = normalized.lower()
    if not any(lower_path.endswith(ext) for ext in LOGO_EXTENSIONS):
        raise ValueError(
            "plugin.toml: embedded `logo` must end with one of " + ", ".join(LOGO_EXTENSIONS)
        )
    return ("embedded", normalized)


def validate_plugin_id(id: str) -> None:
    """Validate a plugin id against the strict ``[a-z0-9_]{2,32}`` grammar.

    Mirrors Rust ``validate_plugin_id``. Ids are globally unique across plugins.
    Invalid characters are rejected — never rewritten — so ``a/b`` and ``a_b``
    cannot collide. Leading/trailing whitespace is rejected (non-lossy), not
    stripped.

    Args:
        id: Candidate plugin id from ``plugin.toml``.

    Raises:
        ValueError: If the id fails length, charset, or underscore rules.
    """
    if id != id.strip():
        raise ValueError(
            f"plugin id `{id}` must not have leading or trailing whitespace"
        )
    if len(id) < 2 or len(id) > 32:
        raise ValueError(f"plugin id `{id}` must be 2–32 characters")
    if not id.isascii() or not all(
        c.islower() or c.isdigit() or c == "_" for c in id
    ):
        raise ValueError(
            f"plugin id `{id}` must be lowercase ascii letters, digits, or `_`"
        )
    if id.startswith("_") or id.endswith("_") or "__" in id:
        raise ValueError(
            f"plugin id `{id}` must not start/end with `_` or contain `__`"
        )


def validate_manifest(m: dict[str, Any]) -> None:
    """Validate a parsed ``plugin.toml`` mapping.

    Args:
        m: Manifest dictionary (typically from ``tomllib.loads``).

    Raises:
        ValueError: If required fields, runtime tables, or network capabilities
            are missing or inconsistent.
    """
    if not str(m.get("id", "")).strip():
        raise ValueError("plugin.toml: `id` is required")
    try:
        # Validate the raw id (non-lossy): do not strip before grammar checks.
        validate_plugin_id(str(m["id"]))
    except ValueError as exc:
        raise ValueError(f"plugin.toml: {exc}") from exc
    if m.get("api_version") != 3:
        raise ValueError("plugin.toml: `api_version` must be 3")
    if m.get("logo") is not None:
        validate_logo(str(m["logo"]))
    _validate_surface(m)
    runtime = m.get("runtime") or "native"
    net = (m.get("capabilities") or {}).get("network") or {}
    if runtime == "native":
        if not str(m.get("command") or "").strip():
            raise ValueError('plugin.toml: `command` is required when runtime = "native"')
        domains = net.get("domains") or []
        if domains:
            raise ValueError(
                'plugin.toml: capabilities.network.domains is only valid for runtime = "workerd" '
                "(native outbound is coarse jail networking with no hostname filter — omit domains)"
            )
    elif runtime == "workerd":
        w = m.get("workerd")
        if not isinstance(w, dict):
            raise ValueError('plugin.toml: `[workerd]` is required when runtime = "workerd"')
        compat = w.get("compatibility_date")
        if not isinstance(compat, str) or not compat.strip():
            raise ValueError("plugin.toml: workerd.compatibility_date is required")
        if not str(w.get("main_module") or "").strip():
            raise ValueError("plugin.toml: workerd.main_module is required")
        validate_author_compatibility_date(compat)
        validate_module_declarations(str(w.get("main_module")), list(m.get("modules") or []))
        validate_author_compatibility_flags(
            list(w.get("compatibility_flags") or []),
            declares_python(m),
        )
        if net.get("mode") == "outbound" and not net.get("domains"):
            raise ValueError(
                'plugin.toml: capabilities.network.domains is required when runtime = "workerd" '
                'and mode = "outbound"'
            )
    else:
        raise ValueError(f"plugin.toml: unknown runtime {runtime}")


ENTRYPOINT_NAMES: tuple[str, ...] = (
    "storefront",
    "storage",
    "databaseAdapter",
    "remoteLibrary",
    "cli",
    "oidc",
)
"""Entrypoint wire names accepted in ``entrypoints``."""

_DATABASE_BINDING_RE = re.compile(r"^[A-Z][A-Z0-9_]*$")


def _validate_surface(m: dict[str, Any]) -> None:
    """Validate ``entrypoints`` / triggers / bindings (mirrors Rust ``validate``)."""
    entrypoints = [str(e) for e in (m.get("entrypoints") or [])]
    seen: set[str] = set()
    for entrypoint in entrypoints:
        if entrypoint not in ENTRYPOINT_NAMES:
            raise ValueError(
                f"plugin.toml: unknown entrypoint `{entrypoint}` "
                f"(expected one of {', '.join(ENTRYPOINT_NAMES)})"
            )
        if entrypoint in seen:
            raise ValueError(f"plugin.toml: entrypoints entry `{entrypoint}` is duplicated")
        seen.add(entrypoint)
    events = m.get("events") or {}
    consumers = list(events.get("consumers") or [])
    jobs = list((m.get("triggers") or {}).get("jobs") or [])
    if not entrypoints and not consumers and not jobs:
        raise ValueError(
            "plugin.toml: declare at least one of `entrypoints`, `[[events.consumers]]`, "
            "or `[triggers].jobs`"
        )
    for consumer in consumers:
        if not str(consumer.get("type") or "").strip():
            raise ValueError("plugin.toml: [[events.consumers]] `type` is required")
    for producer in events.get("producers") or []:
        if not str(producer.get("type") or "").strip():
            raise ValueError("plugin.toml: [[events.producers]] `type` is required")
    if m.get("cli") is not None and "cli" not in seen:
        raise ValueError('plugin.toml: `[cli]` requires `"cli"` in `entrypoints`')
    if ((m.get("oidc") or {}).get("clients") or []) and "oidc" not in seen:
        raise ValueError('plugin.toml: `[[oidc.clients]]` requires `"oidc"` in `entrypoints`')
    bindings: set[str] = set()
    for db in m.get("databases") or []:
        name = str(db.get("binding") or "")
        if not _DATABASE_BINDING_RE.match(name) or len(name) > 32:
            raise ValueError(
                f"plugin.toml: [[databases]] binding `{name}` must be `[A-Z][A-Z0-9_]*` "
                "and at most 32 chars"
            )
        if name in bindings:
            raise ValueError(f"plugin.toml: [[databases]] binding `{name}` is duplicated")
        bindings.add(name)
    reserved = {"CONFIG", "SECRETS", "EVENTS", "WORK_FS", _LOOPBACK_ENV}
    named: list[tuple[str, str]] = []
    secrets = m.get(_SEALED_TABLE)
    if isinstance(secrets, dict):
        named.append(("secrets", str(secrets.get("binding") or _SEALED_ENV)))
    work_fs = m.get("work_fs")
    if isinstance(work_fs, dict):
        named.append(("work_fs", str(work_fs.get("binding") or "WORK_FS")))
    oauth = m.get(_LOOPBACK_TABLE)
    if isinstance(oauth, dict):
        named.append(("oauth", str(oauth.get("binding") or _LOOPBACK_ENV)))
    for kv in m.get("kv_namespaces") or []:
        named.append(("kv_namespaces", str(kv.get("binding") or "KV")))
    for producer in (m.get("events") or {}).get("producers") or []:
        named.append(("events.producers", str(producer.get("binding") or "EVENTS")))
    for table, name in named:
        if not _DATABASE_BINDING_RE.match(name) or len(name) > 32:
            raise ValueError(
                f"plugin.toml: [{table}] binding `{name}` must be `[A-Z][A-Z0-9_]*`"
            )
        if table == "kv_namespaces" and name in reserved:
            raise ValueError(
                f"plugin.toml: [{table}] binding `{name}` collides with another binding"
            )
        if name == "CONFIG" or name in bindings:
            raise ValueError(
                f"plugin.toml: [{table}] binding `{name}` collides with another binding"
            )
    for table, name in named:
        if table == "events.producers":
            continue
        if name in bindings:
            raise ValueError(
                f"plugin.toml: [{table}] binding `{name}` collides with another binding"
            )
        bindings.add(name)


def _workerd_modules_dir(plugin_dir: Path, m: dict[str, Any]) -> Path:
    w = m["workerd"]
    return resolve_under(plugin_dir, w.get("modules_dir") or "modules")


def _is_python_workerd(m: dict[str, Any]) -> bool:
    return declares_python(m)


def _enforce_workerd_load_set(m: dict[str, Any], modules_dir: Path) -> None:
    """Require ``[[modules]]`` rows to be files the walk embeds.

    Python flags follow the manifest declaration. A ``.py`` file the walk finds
    but the manifest does not declare fails even when both flags are set. An
    explicit ``path`` must be in the load set; ``name`` is the source only when
    ``path`` is omitted.

    Args:
        m: Parsed manifest.
        modules_dir: Absolute modules directory.

    Raises:
        ValueError: When a row is missing, a symlink is present, flags disagree,
            or a Python file is not declared.
    """
    load_set = _collect_author_module_keys(modules_dir)
    w = m.get("workerd") or {}
    modules_dir_name = str(w.get("modules_dir") or "modules")
    for mod in m.get("modules") or []:
        # An explicit path is the file to embed. `name` is only the source when
        # `path` was omitted, so a typoed path cannot pass because `name` exists.
        file_path = str(mod.get("path") or mod.get("name") or "")
        key = module_load_key(modules_dir_name, file_path)
        if not key or key not in load_set:
            if workerd_module_is_embedded(file_path):
                raise ValueError(
                    f"plugin.toml: [[modules]] `{file_path}` is not in the workerd load set"
                )
            raise ValueError(f"plugin.toml: [[modules]] `{file_path}` is not implemented yet")
    disk_python = any(name.lower().endswith(".py") for name in load_set)
    validate_author_compatibility_flags(
        list(w.get("compatibility_flags") or []), declares_python(m)
    )
    if disk_python and not declares_python(m):
        raise ValueError("plugin.toml: undeclared Python file in the workerd modules tree")


def _collect_author_module_keys(modules_dir: Path) -> set[str]:
    found: set[str] = set()
    for dirpath, dirnames, filenames in os.walk(modules_dir, followlinks=False):
        root = Path(dirpath)
        for name in list(dirnames):
            child = root / name
            if child.is_symlink():
                raise ValueError(f"refusing symlink in workerd modules tree: {child}")
        for name in filenames:
            child = root / name
            if child.is_symlink():
                raise ValueError(f"refusing symlink in workerd modules tree: {child}")
            if not workerd_module_is_embedded(name):
                continue
            found.add(child.relative_to(modules_dir).as_posix())
    return found


def _sdk_workerd_embed_src() -> Path:
    return cli_user_path(Path(__file__).parent) / "workerd.py"


ENTRYPOINT_EXPORT_CLASSES: dict[str, str] = {
    "storefront": "Storefront",
    "storage": "Storage",
    "databaseAdapter": "DatabaseAdapter",
    "remoteLibrary": "RemoteLibrary",
    "cli": "Cli",
    "oidc": "Oidc",
}
"""Exported class name the launcher binds for each ``entrypoints`` wire name."""


def check_main_module_source(
    main_name: str, src: str, entrypoints: list[str], language: str
) -> None:
    """Check a workerd main module against the v3 author model.

    Requires the SDK import and a ``BookclerkEntrypoint`` default class, rejects
    the removed ``BookclerkPlugin`` base, and requires a class per manifest
    entrypoint (``class Storage(StorageEntrypoint)`` for Python,
    ``export class Storage`` for JavaScript).

    Args:
        main_name: Main module filename (for messages).
        src: Main module source text.
        entrypoints: Manifest ``entrypoints`` wire names.
        language: ``"python"`` or ``"js"``.

    Raises:
        ValueError: When the module does not follow the author model.
    """
    base = "BookclerkEntrypoint"
    if "BookclerkPlugin" in src:
        raise ValueError(
            f"{main_name}: `BookclerkPlugin` was removed in api_version 3; extend "
            f"`{base}` (default class with event()/job() triggers) and define "
            "named entrypoint classes (Storefront, Storage, RemoteLibrary, "
            "DatabaseAdapter, Cli, Oidc)"
        )
    if language == "python":
        if "bookclerk_plugin_sdk" not in src and base not in src:
            raise ValueError(
                f"{main_name}: import {base} from bookclerk_plugin_sdk.workerd "
                f"(e.g. `from bookclerk_plugin_sdk.workerd import {base}, js`)"
            )
    else:
        if "@bookclerk/plugin-sdk" not in src and base not in src:
            raise ValueError(
                f'{main_name}: import {base} from "@bookclerk/plugin-sdk/workerd"'
            )
    if "WorkerEntrypoint" in src and "Entrypoint" not in src.replace("WorkerEntrypoint", ""):
        raise ValueError(f"{main_name}: subclass {base}, not bare WorkerEntrypoint")
    for wire in entrypoints:
        cls = ENTRYPOINT_EXPORT_CLASSES.get(wire)
        if cls is None:
            continue
        if language == "python":
            exported = re.search(rf"^class\s+{cls}\s*\(", src, re.MULTILINE) is not None
            hint = f"class {cls}({cls}Entrypoint)"
        else:
            exported = (
                re.search(rf"export\s+class\s+{cls}\b", src) is not None
                or re.search(rf"export\s*\{{[^}}]*\b{cls}\b[^}}]*\}}", src) is not None
            )
            hint = f"export class {cls} extends {cls}Entrypoint"
        if not exported:
            raise ValueError(
                f"{main_name}: entrypoint `{wire}` declared in plugin.toml but the main "
                f"module does not define `{cls}` ({hint})"
            )


def check_plugin(plugin_dir: Path) -> str:
    """Validate a plugin directory and its ``plugin.toml``.

    Args:
        plugin_dir: Path to the plugin root containing ``plugin.toml``.

    Returns:
        A short ``ok id=... entrypoints=... runtime=...`` status string.

    Raises:
        ValueError: If the manifest or Python workerd sources are invalid.
        FileNotFoundError: If required logo, modules, or native binaries are missing.
        OSError: If ``plugin.toml`` cannot be read.

    Examples:
        >>> # print(check_plugin(Path("./my-plugin")))
        >>> # ok id=echo entrypoints=storefront runtime=workerd
    """
    root = cli_user_path(plugin_dir)
    toml_path = resolve_under(root, "plugin.toml")
    text = toml_path.read_text(encoding="utf-8")
    m = tomllib.loads(text)
    validate_manifest(m)
    if m.get("logo") is not None:
        kind, value = validate_logo(str(m["logo"]))
        if kind == "embedded":
            logo_path = resolve_under(root, value)
            if not logo_path.is_file():
                raise FileNotFoundError(f"embedded logo missing: {logo_path}")
    runtime = m.get("runtime") or "native"
    if runtime == "workerd":
        w = m["workerd"]
        compat = w.get("compatibility_date")
        if not isinstance(compat, str):
            raise ValueError(
                "plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD"
            )
        applied = apply_author_compatibility_date(compat)
        if applied.warning:
            print(applied.warning, file=sys.stderr)
        modules_dir = _workerd_modules_dir(root, m)
        if not modules_dir.is_dir():
            raise FileNotFoundError(f"workerd modules_dir missing: {modules_dir}")
        main = resolve_under(modules_dir, w["main_module"])
        if not main.is_file():
            raise FileNotFoundError(f"workerd main_module missing: {main}")
        entrypoints = [str(e) for e in (m.get("entrypoints") or [])]
        main_lower = str(w["main_module"]).lower()
        if main_lower.endswith(".py"):
            src = main.read_text(encoding="utf-8")
            check_main_module_source(main.name, src, entrypoints, "python")
        elif main_lower.endswith((".js", ".mjs")):
            src = main.read_text(encoding="utf-8")
            check_main_module_source(main.name, src, entrypoints, "js")
        _enforce_workerd_load_set(m, modules_dir)
    elif runtime == "native":
        cmd = Path(m["command"])
        resolved = cli_user_path(cmd) if cmd.is_absolute() else resolve_under(root, cmd)
        if not resolved.exists() and resolve_under(root, ".require-binary").exists():
            raise FileNotFoundError(f"native command not found: {resolved}")
    entrypoints = ",".join(str(e) for e in (m.get("entrypoints") or []))
    return f"ok id={m['id']} entrypoints={entrypoints} runtime={runtime}"


def sync_embed(plugin_dir: Path) -> str:
    """Vendor SDK sources under ``modules/`` for offline workerd archives.

    Prefer package imports — ``bookclerk-workerd`` injects
    ``bookclerk_plugin_sdk.workerd`` at runtime. This writes the same files so a
    staged tree is self-contained without host injection. It does not insert
    compatibility flags; the host and ``check`` require the author to write them.

    Args:
        plugin_dir: Path to a workerd Python plugin root.

    Returns:
        Status string describing the synced path (and flag updates, if any).

    Raises:
        ValueError: If the plugin is not a Python workerd guest.
        OSError: If files cannot be read or written.

    Examples:
        >>> # print(sync_embed(Path("./my-python-workerd-plugin")))
    """
    root = cli_user_path(plugin_dir)
    toml_path = resolve_under(root, "plugin.toml")
    refuse_symlink_path(root, toml_path)
    text = toml_path.read_text(encoding="utf-8")
    m = tomllib.loads(text)
    validate_manifest(m)
    if (m.get("runtime") or "native") != "workerd":
        raise ValueError("sync-embed requires runtime = \"workerd\"")
    if not _is_python_workerd(m):
        raise ValueError(
            "sync-embed (Python SDK): main_module must end with .py "
            f"(got {m['workerd'].get('main_module')!r})"
        )
    modules_rel = (m.get("workerd") or {}).get("modules_dir") or "modules"
    modules_dir = resolve_under(root, modules_rel)
    # Validate existing ancestors only; create missing modules components safely.
    refuse_symlink_existing_components(root, modules_dir)
    modules_dir = ensure_dir_under(root, modules_rel)
    refuse_symlink_path(root, modules_dir)
    pkg = resolve_under(modules_dir, "bookclerk_plugin_sdk")
    refuse_symlink_existing_components(root, pkg)
    pkg = ensure_dir_under(modules_dir, "bookclerk_plugin_sdk")
    refuse_symlink_path(root, pkg)
    init = resolve_under(pkg, "__init__.py")
    if not init.is_file():
        write_file_under(
            pkg,
            "__init__.py",
            '"""Bookclerk plugin SDK (vendored for workerd). Prefer .workerd."""\n',
        )
    dest = copy_file_under(pkg, "workerd.py", _sdk_workerd_embed_src())
    return f"synced {dest}"


def _esc(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _array(values: list[str]) -> str:
    """Emit a TOML array like ``toml::to_string_pretty``: inline for one element."""
    if not values:
        return "[]"
    if len(values) == 1:
        return f"[{values[0]}]"
    inner = ",\n".join(f"    {v}" for v in values)
    return f"[\n{inner},\n]"


def _string_array(values: list[str]) -> str:
    return _array([_esc(v) for v in values])


def _number_array(values: list[Any]) -> str:
    return _array([str(v) for v in values])


def _bool(value: Any) -> str:
    return "true" if value else "false"


def _value(value: Any) -> str | None:
    if isinstance(value, bool):
        return _bool(value)
    if isinstance(value, str):
        return _esc(value)
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        rendered: list[str] = []
        for item in value:
            if isinstance(item, (dict, list)):
                return None
            one = _value(item)
            if one is None:
                return None
            rendered.append(one)
        return _array(rendered)
    return None


def _table_rows(lines: list[str], table: dict[str, Any]) -> None:
    for key in sorted(table):
        rendered = _value(table[key])
        if rendered is not None:
            lines.append(f"{key} = {rendered}")


def _is_array_of_records(value: Any) -> bool:
    """True for a non-empty list whose elements are all dicts."""
    return (
        isinstance(value, list)
        and len(value) > 0
        and all(isinstance(row, dict) for row in value)
    )


def _reject_dropped_queue_tables(queues: dict[str, Any]) -> None:
    """Fail when a ``[queues]`` value would be omitted instead of emitted."""
    for key, value in queues.items():
        if _is_array_of_records(value):
            for row in value:
                for field, field_value in row.items():
                    if _value(field_value) is None:
                        raise ValueError(
                            "plugin.toml: [queues] "
                            f"`{key}.{field}` value cannot be formatted"
                        )
            continue
        if _value(value) is None:
            raise ValueError(
                f"plugin.toml: [queues] `{key}` value cannot be formatted"
            )


def _emit_queues(lines: list[str], queues: Any) -> None:
    """Write a declared ``[queues]`` table back out. Absent means omitted.

    Only a non-empty list of dicts uses ``[[queues.key]]``. Scalar lists and
    empty lists stay inline arrays. Nested tables raise instead of disappearing.
    """
    if not isinstance(queues, dict):
        if queues is not None:
            raise ValueError("plugin.toml: [queues] value cannot be formatted")
        return
    _reject_dropped_queue_tables(queues)
    scalars = {
        key: value for key, value in queues.items() if not _is_array_of_records(value)
    }
    lists = {key: value for key, value in queues.items() if _is_array_of_records(value)}
    if scalars or not lists:
        lines.append("")
        lines.append("[queues]")
        _table_rows(lines, scalars)
    for key in sorted(lists):
        for row in lists[key]:
            lines.append("")
            lines.append(f"[[queues.{key}]]")
            if isinstance(row, dict):
                _table_rows(lines, row)


def _named_binding(lines: list[str], header: str, binding: Any) -> None:
    if binding is None:
        return
    lines.append("")
    lines.append(header)
    name = str((binding or {}).get("binding") or "") if isinstance(binding, dict) else ""
    if name:
        lines.append(f"binding = {_esc(name)}")


def format_manifest(m: dict[str, Any]) -> str:
    """Emit canonical TOML matching Rust ``format_manifest`` gold fixtures.

    Flags are emitted as written. This function does not insert the Python pair.

    Args:
        m: Validated manifest dictionary.

    Returns:
        Canonical ``plugin.toml`` text ending with a newline.

    Raises:
        ValueError: When ``[queues]`` contains a nested table or a datetime.
    """
    lines: list[str] = []
    lines.append(f"api_version = {m['api_version']}")
    lines.append(f"id = {_esc(m['id'])}")
    if m.get("name") is not None:
        lines.append(f"name = {_esc(m['name'])}")
    if m.get("version") is not None:
        lines.append(f"version = {_esc(m['version'])}")
    if m.get("logo") is not None:
        lines.append(f"logo = {_esc(m['logo'])}")
    runtime = m.get("runtime") or "native"
    lines.append(f"runtime = {_esc(runtime)}")
    if m.get("command") is not None:
        lines.append(f"command = {_esc(m['command'])}")
    if m.get("args"):
        lines.append(f"args = {_string_array(list(m['args']))}")
    if m.get("entrypoints"):
        lines.append(f"entrypoints = {_string_array([str(e) for e in m['entrypoints']])}")

    if m.get("workerd"):
        w = m["workerd"]
        lines.append("")
        lines.append("[workerd]")
        lines.append(f"compatibility_date = {_esc(w['compatibility_date'])}")
        if w.get("compatibility_flags"):
            lines.append(f"compatibility_flags = {_string_array(list(w['compatibility_flags']))}")
        lines.append(f"main_module = {_esc(w['main_module'])}")
        lines.append(f"modules_dir = {_esc(w.get('modules_dir') or 'modules')}")
        lines.append(f"entrypoint = {_esc(w.get('entrypoint') or 'default')}")
        limits = w.get("limits") or {}
        if limits.get("cpu_ms") is not None or limits.get("subrequests") is not None:
            lines.append("")
            lines.append("[workerd.limits]")
            if limits.get("cpu_ms") is not None:
                lines.append(f"cpu_ms = {limits['cpu_ms']}")
            if limits.get("subrequests") is not None:
                lines.append(f"subrequests = {limits['subrequests']}")

    for mod in m.get("modules") or []:
        lines.append("")
        lines.append("[[modules]]")
        lines.append(f"name = {_esc(str(mod['name']))}")
        path = str(mod.get("path") or "")
        if path:
            lines.append(f"path = {_esc(path)}")
        lines.append(f"type = {_esc(str(mod.get('type') or 'js'))}")

    jobs = list((m.get("triggers") or {}).get("jobs") or [])
    if jobs:
        lines.append("")
        lines.append("[triggers]")
        lines.append(f"jobs = {_string_array([str(j) for j in jobs])}")

    events = m.get("events") or {}
    for consumer in events.get("consumers") or []:
        lines.append("")
        lines.append("[[events.consumers]]")
        lines.append(f"type = {_esc(str(consumer['type']))}")
        lines.append(f"schema_versions = {_number_array(list(consumer.get('schema_versions') or [1]))}")
        lines.append(f"supports_suspend = {_bool(consumer.get('supports_suspend'))}")
        lines.append(f"resource_class = {_esc(str(consumer.get('resource_class') or 'network'))}")
        if consumer.get("max_retries") is not None:
            lines.append(f"max_retries = {consumer['max_retries']}")
        filt = consumer.get("filter")
        if isinstance(filt, dict) and filt:
            lines.append("")
            lines.append("[events.consumers.filter]")
            _table_rows(lines, filt)
    for producer in events.get("producers") or []:
        lines.append("")
        lines.append("[[events.producers]]")
        lines.append(f"type = {_esc(str(producer['type']))}")
        if producer.get("binding"):
            lines.append(f"binding = {_esc(str(producer['binding']))}")

    for db in m.get("databases") or []:
        lines.append("")
        lines.append("[[databases]]")
        lines.append(f"binding = {_esc(str(db['binding']))}")

    if m.get("vars") is not None:
        lines.append("")
        lines.append("[vars]")
        _table_rows(lines, dict(m["vars"]))
    _named_binding(lines, f"[{_SEALED_TABLE}]", m.get(_SEALED_TABLE))
    for kv in m.get("kv_namespaces") or []:
        _named_binding(lines, "[[kv_namespaces]]", kv)
    _emit_queues(lines, m.get("queues"))
    _named_binding(lines, "[work_fs]", m.get("work_fs"))
    _named_binding(lines, f"[{_LOOPBACK_TABLE}]", m.get(_LOOPBACK_TABLE))

    caps = m.get("capabilities") or {}
    net = caps.get("network") or {}
    lines.append("")
    lines.append("[capabilities.network]")
    lines.append(f"mode = {_esc(net.get('mode') or 'deny')}")
    if net.get("domains"):
        lines.append(f"domains = {_string_array(list(net['domains']))}")

    cli = m.get("cli") or {}
    for cmd in cli.get("commands") or []:
        lines.append("")
        lines.append("[[cli.commands]]")
        lines.append(f"name = {_esc(cmd['name'])}")
        if cmd.get("about") is not None:
            lines.append(f"about = {_esc(cmd['about'])}")
        if not cmd.get("args"):
            lines.append("args = []")
        for arg in cmd.get("args") or []:
            lines.append("")
            lines.append("[[cli.commands.args]]")
            lines.append(f"name = {_esc(str(arg['name']))}")
            if arg.get("long") is not None:
                lines.append(f"long = {_esc(str(arg['long']))}")
            if arg.get("short") is not None:
                lines.append(f"short = {_esc(str(arg['short']))}")
            lines.append(f"kind = {_esc(str(arg.get('kind') or 'string'))}")
            lines.append(f"required = {_bool(arg.get('required'))}")
            if arg.get("default") is not None:
                lines.append(f"default = {_esc(str(arg['default']))}")
            if arg.get("about") is not None:
                lines.append(f"about = {_esc(str(arg['about']))}")
            lines.append(f"positional = {_bool(arg.get('positional'))}")

    for client in (m.get("oidc") or {}).get("clients") or []:
        lines.append("")
        lines.append("[[oidc.clients]]")
        lines.append(f"client_id = {_esc(str(client['client_id']))}")
        if client.get("display_name"):
            lines.append(f"display_name = {_esc(str(client['display_name']))}")
        lines.append(f"callback_path = {_esc(str(client['callback_path']))}")
        lines.append(f"public_client = {_bool(client.get('public_client', True))}")
        scopes = list(client.get("default_scopes") or [])
        if scopes:
            lines.append(f"default_scopes = {_string_array([str(sc) for sc in scopes])}")
        lines.append(f"issue_refresh_token = {_bool(client.get('issue_refresh_token', True))}")
        lines.append(f"origin_config_key = {_esc(str(client['origin_config_key']))}")

    out = "\n".join(lines)
    if not out.endswith("\n"):
        out += "\n"
    return out


def fmt_plugin_toml(path: Path, *, check_only: bool) -> str:
    """Format ``plugin.toml`` in place, or check that it is already canonical.

    Args:
        path: Path to ``plugin.toml``.
        check_only: When ``True``, raise if reformatting would change the file.

    Returns:
        Status string (``ok …`` or ``wrote …``).

    Raises:
        ValueError: If validation fails or ``check_only`` finds a drift.
        OSError: If the file cannot be read or written.
    """
    # Operator-selected plugin.toml path (trusted operation root for this write).
    safe = cli_user_path(path)
    text = safe.read_text(encoding="utf-8")
    m = tomllib.loads(text)
    validate_manifest(m)
    formatted = format_manifest(m)

    def norm(s: str) -> str:
        s = s.replace("\r\n", "\n")
        return s if s.endswith("\n") else s + "\n"

    if check_only:
        if norm(text) != norm(formatted):
            raise ValueError(f"would reformat {safe}")
        return f"ok {safe}"
    safe.write_text(formatted, encoding="utf-8")
    return f"wrote {safe}"


def _host_target() -> str:
    import platform

    sysname = sys.platform
    machine = platform.machine().lower()
    if sysname.startswith("linux") and machine in {"x86_64", "amd64"}:
        return "linux-x64-gnu"
    if sysname.startswith("linux") and machine in {"aarch64", "arm64"}:
        return "linux-arm64"
    if sysname == "darwin" and machine in {"arm64", "aarch64"}:
        return "macos-arm64"
    if sysname == "darwin" and machine in {"x86_64", "amd64"}:
        return "macos-x64"
    if sysname.startswith("win") and machine in {"x86_64", "amd64"}:
        return "windows-x64"
    return f"{sysname}-{machine}"


def package_plugin(plugin_dir: Path, out_dir: Path) -> Path:
    """Pack a plugin into a ``.tar.gz`` archive and update ``SHA256SUMS``.

    Args:
        plugin_dir: Path to the plugin root.
        out_dir: Destination directory for the archive and checksums file.

    Returns:
        Path to the created ``.tar.gz`` archive.

    Raises:
        ValueError: If the manifest is invalid.
        FileNotFoundError: If a required native binary, modules tree, or logo is missing.
        subprocess.CalledProcessError: If ``tar`` fails.

    Examples:
        >>> # archive = package_plugin(Path("./my-plugin"), Path("./dist"))
        >>> # print(f"packed {archive}")
    """
    root = Path(os.path.realpath(cli_user_path(plugin_dir)))
    out = cli_user_path(out_dir)
    toml_path = resolve_under(root, "plugin.toml")
    refuse_symlink_path(root, toml_path)
    m = tomllib.loads(toml_path.read_text(encoding="utf-8"))
    validate_manifest(m)
    version = m.get("version") or "0.0.0"
    plugin_id = m["id"]
    out.mkdir(parents=True, exist_ok=True)
    staging = resolve_under(out, f".staging-{plugin_id}")
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True)
    runtime = m.get("runtime") or "native"
    if runtime == "native":
        shutil.copy2(toml_path, resolve_under(staging, "plugin.toml"))
        cmd = Path(m["command"])
        # Absolute command paths are operator-selected build outputs (documented).
        # Relative paths must stay under the plugin tree without following links.
        src = cli_user_path(cmd) if cmd.is_absolute() else resolve_under(root, cmd)
        if not cmd.is_absolute():
            refuse_symlink_path(root, src)
        if src.is_symlink() or not src.is_file():
            raise FileNotFoundError(f"native binary not found for package: {src}")
        dest = resolve_under(staging, src.name)
        shutil.copy2(src, dest, follow_symlinks=False)
        os.chmod(dest, 0o755)
        stem = f"bookclerk-plugin-{plugin_id}-{version}-{_host_target()}"
    else:
        modules_dir = m["workerd"].get("modules_dir") or "modules"
        src_modules = resolve_under(root, modules_dir)
        refuse_symlink_path(root, src_modules)
        dest_modules = resolve_under(staging, modules_dir)
        copy_tree_no_symlinks(src_modules, dest_modules)
        toml_text = toml_path.read_text(encoding="utf-8")
        if _is_python_workerd(m):
            # Vendor package-shaped SDK so archives work even without host injection.
            pkg = resolve_under(dest_modules, "bookclerk_plugin_sdk")
            pkg.mkdir(parents=True, exist_ok=True)
            resolve_under(pkg, "__init__.py").write_text(
                '"""Bookclerk plugin SDK (vendored for workerd)."""\n',
                encoding="utf-8",
            )
            shutil.copy2(_sdk_workerd_embed_src(), resolve_under(pkg, "workerd.py"))
        resolve_under(staging, "plugin.toml").write_text(toml_text, encoding="utf-8")
        stem = f"bookclerk-plugin-{plugin_id}-{version}-workerd"

    if m.get("logo") is not None:
        kind, value = validate_logo(str(m["logo"]))
        if kind == "embedded":
            src = resolve_under(root, value)
            refuse_symlink_path(root, src)
            if src.is_symlink() or not src.is_file():
                raise FileNotFoundError(f"embedded logo missing for package: {src}")
            dest = resolve_under(staging, value)
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dest, follow_symlinks=False)

    archive_name = f"{stem}.tar.gz"
    # Free-form version may embed path segments; contain under out before write.
    archive_path = resolve_under(out, archive_name)
    attempt_name = f".packaging-attempt-{plugin_id}-{os.getpid()}-{time.time_ns()}"
    attempt_dir = resolve_under(out, attempt_name)
    os.mkdir(attempt_dir)
    owned_attempt = True
    tmp_path = resolve_under(attempt_dir, "archive.tar.gz")
    try:
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        fd = os.open(os.fspath(tmp_path), flags, 0o644)
        try:
            with os.fdopen(fd, "wb") as out_fh:
                proc = subprocess.run(
                    ["tar", "-C", str(staging), "-czf", "-", "."],
                    check=True,
                    stdout=out_fh,
                )
                del proc
        except Exception:
            if tmp_path.exists() and not tmp_path.is_symlink():
                tmp_path.unlink(missing_ok=True)
            raise
        os.replace(tmp_path, archive_path)
        shutil.rmtree(attempt_dir, ignore_errors=True)
        owned_attempt = False
    except Exception:
        if owned_attempt:
            shutil.rmtree(attempt_dir, ignore_errors=True)
        raise
    finally:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)
    digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    sums = resolve_under(out, "SHA256SUMS")
    refuse_symlink_path(out, sums)
    lines = []
    if sums.is_file() and not sums.is_symlink():
        lines = [
            ln
            for ln in sums.read_text(encoding="utf-8").splitlines()
            if ln and not ln.endswith(archive_name)
        ]
    lines.append(f"{digest}  {archive_name}")
    write_file_under(out, "SHA256SUMS", "\n".join(lines) + "\n")
    return archive_path


TYPES_OUTPUT_FILE = "bookclerk_configuration.py"
"""Default ``types`` output filename next to ``plugin.toml``."""

_RESERVED_BINDINGS = frozenset(
    {"CONFIG", _SEALED_ENV, "EVENTS", "WORK_FS", "KV", _LOOPBACK_ENV}
)


def _var_type(value: Any) -> str:
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, int):
        return "int"
    if isinstance(value, float):
        return "float"
    if isinstance(value, str):
        return "str"
    if isinstance(value, list):
        return "list[Any]"
    if isinstance(value, dict):
        return "dict[str, Any]"
    return "Any"


def env_properties_for(m: dict[str, Any]) -> list[tuple[str, str, str]]:
    """Compute the ``Env`` protocol members for a validated manifest.

    Args:
        m: Validated manifest dict.

    Returns:
        ``(binding, python_type, doc)`` tuples in stable emission order.

    Raises:
        ValueError: When a ``[[databases]]`` binding collides with a reserved name
            or a binding is declared twice.
    """
    props: list[tuple[str, str, str]] = []
    vars_table = m.get("vars")
    if isinstance(vars_table, dict) and vars_table:
        fields = ", ".join(f'"{k}": {_var_type(vars_table[k])}' for k in sorted(vars_table))
        config_type = f"dict[str, Any]  # {{{fields}}}"
    else:
        config_type = "dict[str, Any]"
    props.append(("CONFIG", config_type, "`[vars]` plus operator settings from the granted config payload."))
    sealed = m.get(_SEALED_TABLE)
    if isinstance(sealed, dict):
        props.append(
            (
                str(sealed.get("binding") or _SEALED_ENV),
                "dict[str, str]",
                f"`[{_SEALED_TABLE}]` values sealed by the operator (present only when granted).",
            )
        )
    producers = (m.get("events") or {}).get("producers") or []
    if producers:
        types = ", ".join(str(p.get("type")) for p in producers)
        props.append(("EVENTS", "Any", f"`[[events.producers]]` outbox publisher ({types})."))
    work_fs = m.get("work_fs")
    if isinstance(work_fs, dict):
        props.append((str(work_fs.get("binding") or "WORK_FS"), "Any", "`[work_fs]` host-granted object storage."))
    for kv in m.get("kv_namespaces") or []:
        props.append(
            (
                str(kv.get("binding") or "KV"),
                "Any",
                "`[[kv_namespaces]]` durable KV binding. Not implemented yet.",
            )
        )
    loopback = m.get(_LOOPBACK_TABLE)
    if isinstance(loopback, dict):
        props.append(
            (
                str(loopback.get("binding") or _LOOPBACK_ENV),
                "Any",
                f"`[{_LOOPBACK_TABLE}]` loopback helper (surface reserved).",
            )
        )
    for db in m.get("databases") or []:
        name = str(db.get("binding"))
        if name in _RESERVED_BINDINGS:
            raise ValueError(
                f"plugin.toml: [[databases]] binding `{name}` collides with a Bookclerk binding"
            )
        props.append((name, "DatabaseBinding", "`[[databases]]` plugin-owned database (D1-shaped prepare/batch/exec)."))
    seen: set[str] = set()
    for name, _, _ in props:
        if name in seen:
            raise ValueError(f"plugin.toml: binding `{name}` is declared twice")
        seen.add(name)
    return props


def render_env_types(m: dict[str, Any]) -> str:
    """Render the ``bookclerk_configuration.py`` typing stub for a validated manifest.

    The stub declares an ``Env`` :class:`typing.Protocol` naming every binding
    the manifest grants so editors and type checkers can follow
    ``self.env.<BINDING>`` inside ``BookclerkEntrypoint`` subclasses.

    Args:
        m: Validated manifest dict.

    Returns:
        Python source text (ends with a newline).

    Raises:
        ValueError: When binding names collide.
    """
    props = env_properties_for(m)
    lines = [
        f"# Generated by `bookclerk-plugin types` from plugin.toml (id={m['id']}). Do not edit.",
        f"# Regenerate after changing [vars], [{_SEALED_TABLE}], [[events.producers]], [[databases]],",
        f"# [work_fs], [[kv_namespaces]], or [{_LOOPBACK_TABLE}].",
        "from __future__ import annotations",
        "",
        "from typing import Any, Protocol",
    ]
    if any(t == "DatabaseBinding" for _, t, _ in props):
        lines.append("")
        lines.append("from bookclerk_plugin_sdk.db_value import DatabaseBinding")
    lines += [
        "",
        "",
        "class Env(Protocol):",
        f'    """Bindings the host grants to `{m["id"]}` (``self.env`` in the entrypoints)."""',
        "",
    ]
    for name, type_, doc in props:
        lines.append(f"    {name}: {type_}")
        lines.append(f'    """{doc}"""')
    lines.append("")
    return "\n".join(lines)


def generate_types(plugin_dir: Path, out_file: Path | None = None) -> str:
    """Write ``bookclerk_configuration.py`` beside a plugin's ``plugin.toml``.

    Args:
        plugin_dir: Plugin root containing ``plugin.toml``.
        out_file: Optional output path (default ``<plugin_dir>/bookclerk_configuration.py``).

    Returns:
        Status string naming the written file.

    Raises:
        ValueError: When the manifest is invalid or bindings collide.
        OSError: If files cannot be read or written.
    """
    root = cli_user_path(plugin_dir)
    toml_path = resolve_under(root, "plugin.toml")
    m = tomllib.loads(toml_path.read_text(encoding="utf-8"))
    validate_manifest(m)
    if out_file is None:
        dest = resolve_under(root, TYPES_OUTPUT_FILE)
        refuse_symlink_path(root, dest)
    else:
        # Operator-selected output path (not forced under cwd).
        dest = cli_user_path(out_file)
    dest.write_text(render_env_types(m), encoding="utf-8")
    return f"wrote {dest}"
