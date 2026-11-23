"""Canonical ``plugin.toml`` emit, matching the Rust formatter byte for byte.

``bookclerk-plugin-manifest`` formats a manifest with ``toml::to_string_pretty``
over its ``PluginManifest`` struct. This module reproduces that in two steps,
like the TypeScript SDK: :func:`canonical_manifest` rebuilds the manifest the
way the Rust struct serializes (fields in struct order, serde defaults filled
in, fields the Rust side skips left out), and :func:`emit_toml` writes that
tree the way ``toml`` pretty-prints it (a table's plain values first, then its
tables and arrays of tables; arrays of more than one element one per line).

The shared corpus under
``crates/bookclerk-plugin-abi/fixtures/tools/fmt-corpus/`` holds the Rust
output for every first-party manifest, and the SDK tests compare against it.
"""

from __future__ import annotations

import math
import re
from collections.abc import Callable
from typing import Any

Table = dict[str, Any]
"""A TOML table in emit order (insertion order is the key order)."""

_BARE_KEY = re.compile(r"^[A-Za-z0-9_-]+$")
"""Keys TOML can write without quotes."""

_SKIP = object()
"""Marker for a field the Rust side does not serialize."""


def _table_at(src: Table, key: str) -> Table | None:
    """Return the child table at ``key``, or ``None``.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The child table when it is a dict.
    """
    value = src.get(key)
    return value if isinstance(value, dict) else None


def _list_at(src: Table, key: str) -> list[Any]:
    """Return the list at ``key``, or an empty list.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The list.
    """
    value = src.get(key)
    return value if isinstance(value, list) else []


def _table(entries: list[tuple[str, Any]]) -> Table:
    """Build one canonical table, dropping skipped fields.

    Args:
        entries: ``(key, value)`` pairs in Rust struct order; a value of
            ``_SKIP`` is left out.

    Returns:
        The table.
    """
    return {key: value for key, value in entries if value is not _SKIP}


def _opt_str(src: Table, key: str) -> Any:
    """Return an ``Option<String>`` field, skipped when absent.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The string, or ``_SKIP``.
    """
    value = src.get(key)
    return value if isinstance(value, str) else _SKIP


def _non_empty_str(src: Table, key: str) -> Any:
    """Return a ``String`` field skipped when empty.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The string, or ``_SKIP`` when empty or absent.
    """
    value = src.get(key)
    return value if isinstance(value, str) and value else _SKIP


def _str(src: Table, key: str, fallback: str = "") -> str:
    """Return a required string field (serialized even when empty).

    Args:
        src: Parent table.
        key: Field name.
        fallback: Serde default when absent.

    Returns:
        The string.
    """
    value = src.get(key)
    return value if isinstance(value, str) else fallback


def _bool(src: Table, key: str, fallback: bool = False) -> bool:
    """Return a boolean field with its serde default.

    Args:
        src: Parent table.
        key: Field name.
        fallback: Default when absent.

    Returns:
        The boolean.
    """
    value = src.get(key)
    return value if isinstance(value, bool) else fallback


def _true_only(src: Table, key: str) -> Any:
    """Return a boolean field skipped when ``false``.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        ``True``, or ``_SKIP``.
    """
    return True if src.get(key) is True else _SKIP


def _opt_int(src: Table, key: str) -> Any:
    """Return an integer field, skipped when absent.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The integer, or ``_SKIP``.
    """
    value = src.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        return _SKIP
    return value


def _int(src: Table, key: str, fallback: int) -> int:
    """Return an integer field with its serde default.

    Args:
        src: Parent table.
        key: Field name.
        fallback: Default when absent.

    Returns:
        The integer.
    """
    value = _opt_int(src, key)
    return fallback if value is _SKIP else int(value)


def _int_unless(src: Table, key: str, omitted: int) -> Any:
    """Return an integer field skipped when it equals the omitted default.

    Args:
        src: Parent table.
        key: Field name.
        omitted: The value the Rust side does not serialize.

    Returns:
        The integer, or ``_SKIP``.
    """
    value = _opt_int(src, key)
    return _SKIP if value is _SKIP or value == omitted else value


def _strings(src: Table, key: str) -> list[str]:
    """Return a list of strings (serialized even when empty).

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The strings.
    """
    return [str(v) for v in _list_at(src, key)]


def _non_empty_strings(src: Table, key: str) -> Any:
    """Return a list of strings skipped when empty.

    Args:
        src: Parent table.
        key: Field name.

    Returns:
        The strings, or ``_SKIP``.
    """
    out = _strings(src, key)
    return out if out else _SKIP


def _tables(src: Table, key: str, each: Callable[[Table], Table]) -> list[Table]:
    """Return the canonical form of each table in a list.

    Args:
        src: Parent table.
        key: Field name.
        each: Canonical form of one entry.

    Returns:
        The tables.
    """
    return [each(entry) for entry in _list_at(src, key) if isinstance(entry, dict)]


def _non_empty_tables(src: Table, key: str, each: Callable[[Table], Table]) -> Any:
    """Return a list of canonical tables skipped when empty.

    Args:
        src: Parent table.
        key: Field name.
        each: Canonical form of one entry.

    Returns:
        The tables, or ``_SKIP``.
    """
    out = _tables(src, key, each)
    return out if out else _SKIP


def _any_value(value: Any) -> Any:
    """Re-serialize a free-form value the way the Rust side does.

    Object keys are sorted (``BTreeMap``); everything else is kept as parsed.

    Args:
        value: Parsed value.

    Returns:
        The canonical value.
    """
    if isinstance(value, dict):
        return {key: _any_value(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        return [_any_value(item) for item in value]
    return value


def _non_empty_table(table: Table) -> Any:
    """Return ``table``, or ``_SKIP`` when it has no fields.

    Args:
        table: Canonical table.

    Returns:
        The table, or ``_SKIP``.
    """
    return table if table else _SKIP


def _named_binding(src: Table | None) -> Any:
    """Return a canonical ``NamedBinding`` (``[secrets]``, ``[work_fs]``, ...).

    Args:
        src: Parsed table, or ``None`` when absent.

    Returns:
        The table, or ``_SKIP`` when absent.
    """
    if src is None:
        return _SKIP
    return _table([("binding", _non_empty_str(src, "binding"))])


def _setting_spec(src: Table) -> Table:
    """Return a canonical ``SettingSpec`` (also used for table columns).

    Args:
        src: Parsed ``[[settings]]`` entry.

    Returns:
        The table.
    """
    return _table(
        [
            ("key", _str(src, "key")),
            ("label", _non_empty_str(src, "label")),
            ("type", _str(src, "type")),
            (
                "values",
                _non_empty_tables(
                    src,
                    "values",
                    lambda v: _table(
                        [("id", _str(v, "id")), ("label", _non_empty_str(v, "label"))]
                    ),
                ),
            ),
            ("default", _any_value(src["default"]) if "default" in src else _SKIP),
            ("required", _true_only(src, "required")),
            ("description", _opt_str(src, "description")),
            ("columns", _non_empty_tables(src, "columns", _setting_spec)),
            ("network", _opt_str(src, "network")),
            ("default_port", _opt_int(src, "default_port")),
        ]
    )


def _webhook(src: Table) -> Table:
    """Return a canonical ``WebhookManifest`` with its scheme-tagged signature.

    Args:
        src: Parsed ``[[webhooks]]`` entry.

    Returns:
        The table.
    """
    sig = _table_at(src, "signature") or {}
    scheme = _str(sig, "scheme")
    if scheme == "hmac-sha256":
        signature = _table(
            [
                ("scheme", scheme),
                ("header", _str(sig, "header")),
                ("prefix", _non_empty_str(sig, "prefix")),
                ("encoding", _str(sig, "encoding", "hex")),
                ("secret_setting", _str(sig, "secret_setting")),
            ]
        )
    else:
        signature = {"scheme": scheme}
    return _table(
        [
            ("name", _str(src, "name")),
            ("max_body_bytes", _int_unless(src, "max_body_bytes", 64 * 1024)),
            ("rate_per_minute", _int_unless(src, "rate_per_minute", 60)),
            ("retention_secs", _int_unless(src, "retention_secs", 86_400)),
            ("headers", _non_empty_strings(src, "headers")),
            ("signature", signature),
        ]
    )


def _workerd(src: Table | None) -> Any:
    """Return the canonical ``[workerd]`` table.

    Args:
        src: Parsed table, or ``None`` when absent.

    Returns:
        The table, or ``_SKIP``.
    """
    if src is None:
        return _SKIP
    limits = _table_at(src, "limits")
    limits_table: Any = _SKIP
    if limits is not None:
        limits_table = _non_empty_table(
            _table(
                [
                    ("cpu_ms", _opt_int(limits, "cpu_ms")),
                    ("subrequests", _opt_int(limits, "subrequests")),
                ]
            )
        )
    return _table(
        [
            ("compatibility_date", _str(src, "compatibility_date")),
            ("compatibility_flags", _non_empty_strings(src, "compatibility_flags")),
            ("main_module", _str(src, "main_module")),
            ("modules_dir", _str(src, "modules_dir", "modules")),
            ("entrypoint", _str(src, "entrypoint", "default")),
            ("limits", limits_table),
        ]
    )


def _events(src: Table) -> Any:
    """Return the canonical ``[events]`` table.

    Args:
        src: Parsed table (empty when absent).

    Returns:
        The table, or ``_SKIP`` when it has no consumers or producers.
    """

    def consumer(c: Table) -> Table:
        versions = [1] if "schema_versions" not in c else _any_value(c["schema_versions"])
        return _table(
            [
                ("type", _str(c, "type")),
                ("schema_versions", versions),
                ("supports_suspend", _bool(c, "supports_suspend")),
                ("resource_class", _str(c, "resource_class", "network")),
                ("filter", _any_value(c["filter"]) if "filter" in c else _SKIP),
                ("max_retries", _opt_int(c, "max_retries")),
                ("message_retention_period", _opt_int(c, "message_retention_period")),
            ]
        )

    def producer(p: Table) -> Table:
        return _table([("type", _str(p, "type")), ("binding", _non_empty_str(p, "binding"))])

    return _non_empty_table(
        _table(
            [
                ("consumers", _non_empty_tables(src, "consumers", consumer)),
                ("producers", _non_empty_tables(src, "producers", producer)),
            ]
        )
    )


def _queues(src: Table) -> Any:
    """Return the canonical ``[queues]`` table.

    Args:
        src: Parsed table (empty when absent).

    Returns:
        The table, or ``_SKIP`` when it has no producers or consumers.
    """

    def producer(p: Table) -> Table:
        return {"queue": _str(p, "queue"), "binding": _str(p, "binding")}

    def consumer(c: Table) -> Table:
        return _table(
            [
                ("queue", _str(c, "queue")),
                ("max_batch_size", _int(c, "max_batch_size", 10)),
                ("max_batch_timeout", _int(c, "max_batch_timeout", 5)),
                ("max_retries", _int(c, "max_retries", 3)),
                ("dead_letter_queue", _opt_str(c, "dead_letter_queue")),
                ("retry_delay", _int(c, "retry_delay", 0)),
                ("max_concurrency", _opt_int(c, "max_concurrency")),
                ("message_retention_period", _opt_int(c, "message_retention_period")),
            ]
        )

    return _non_empty_table(
        _table(
            [
                ("producers", _non_empty_tables(src, "producers", producer)),
                ("consumers", _non_empty_tables(src, "consumers", consumer)),
            ]
        )
    )


def _media_tables(m: Table) -> list[tuple[str, Any]]:
    """Return the canonical media, acquire, metadata-file and notification tables.

    Args:
        m: Parsed manifest.

    Returns:
        ``(key, value)`` pairs in Rust struct order.
    """
    media = _table_at(m, "media")
    acquire = _table_at(m, "acquire")
    metadata_files = _table_at(m, "metadata_files")
    notifier = _table_at(m, "notifier")
    types = [
        t for t in _list_at(_table_at(m, "notifications") or {}, "types") if isinstance(t, dict)
    ]

    def dialect(d: Table) -> Table:
        return _table(
            [
                ("id", _str(d, "id")),
                ("name", _str(d, "name")),
                ("file_names", _non_empty_strings(d, "file_names")),
                ("suffixes", _non_empty_strings(d, "suffixes")),
                ("fields", _strings(d, "fields")),
                ("credit_roles", _non_empty_strings(d, "credit_roles")),
                ("writes", _bool(d, "writes")),
            ]
        )

    def address_field(f: Table) -> Table:
        return {
            "key": _str(f, "key"),
            "label": _str(f, "label"),
            "required": _bool(f, "required"),
            "secret": _bool(f, "secret"),
        }

    def notification_type(t: Table) -> Table:
        return {
            "id": _str(t, "id"),
            "audience": _str(t, "audience"),
            "severity": _str(t, "severity", "info"),
            "title": _str(t, "title"),
        }

    return [
        (
            "media",
            _SKIP
            if media is None
            else _table(
                [
                    ("works", _strings(media, "works")),
                    ("lists", _strings(media, "lists")),
                    ("fetches", _non_empty_strings(media, "fetches")),
                ]
            ),
        ),
        (
            "acquire",
            _SKIP
            if acquire is None
            else _table(
                [
                    ("downloads_per_day", _opt_int(acquire, "downloads_per_day")),
                    ("min_gap_seconds", _opt_int(acquire, "min_gap_seconds")),
                    ("cooldown_seconds", _opt_int(acquire, "cooldown_seconds")),
                ]
            ),
        ),
        (
            "metadata_files",
            _SKIP
            if metadata_files is None
            else {"dialects": _tables(metadata_files, "dialects", dialect)},
        ),
        (
            "notifier",
            _SKIP
            if notifier is None
            else _table(
                [
                    ("destinations", _strings(notifier, "destinations")),
                    ("account_mail", _bool(notifier, "account_mail")),
                    (
                        "formats",
                        ["text"] if "formats" not in notifier else _strings(notifier, "formats"),
                    ),
                    (
                        "address_fields",
                        _non_empty_tables(notifier, "address_fields", address_field),
                    ),
                ]
            ),
        ),
        (
            "notifications",
            {"types": [notification_type(t) for t in types]} if types else _SKIP,
        ),
    ]


def _capabilities(m: Table) -> Table:
    """Return the canonical ``[capabilities]`` table (always serialized).

    Args:
        m: Parsed manifest.

    Returns:
        The table.
    """
    network = _table_at(_table_at(m, "capabilities") or {}, "network") or {}
    own = _table_at(network, "own_user_agent")

    def tcp(t: Table) -> Table:
        return {"host": _str(t, "host"), "ports": _any_value(_list_at(t, "ports"))}

    return {
        "network": _table(
            [
                ("mode", _str(network, "mode", "deny")),
                ("domains", _non_empty_strings(network, "domains")),
                ("tcp", _non_empty_tables(network, "tcp", tcp)),
                (
                    "allow_undeclared_public_redirects",
                    _true_only(network, "allow_undeclared_public_redirects"),
                ),
                ("address_cidrs", _non_empty_strings(network, "address_cidrs")),
                (
                    "own_user_agent",
                    _SKIP
                    if own is None
                    else {"hosts": _strings(own, "hosts"), "reason": _str(own, "reason")},
                ),
            ]
        )
    }


def _cli(src: Table | None) -> Any:
    """Return the canonical ``[cli]`` table.

    Args:
        src: Parsed table, or ``None`` when absent.

    Returns:
        The table, or ``_SKIP``.
    """
    if src is None:
        return _SKIP

    def arg(a: Table) -> Table:
        return _table(
            [
                ("name", _str(a, "name")),
                ("long", _opt_str(a, "long")),
                ("short", _opt_str(a, "short")),
                ("kind", _str(a, "kind", "string")),
                ("required", _bool(a, "required")),
                ("default", _opt_str(a, "default")),
                ("about", _opt_str(a, "about")),
                ("positional", _bool(a, "positional")),
            ]
        )

    def command(c: Table) -> Table:
        return _table(
            [
                ("name", _str(c, "name")),
                ("about", _opt_str(c, "about")),
                ("args", _tables(c, "args", arg)),
            ]
        )

    return {"commands": _tables(src, "commands", command)}


def _oidc(src: Table) -> Any:
    """Return the canonical ``[oidc]`` table.

    Args:
        src: Parsed table (empty when absent).

    Returns:
        The table, or ``_SKIP`` when it has no clients.
    """

    def client(c: Table) -> Table:
        return _table(
            [
                ("client_id", _str(c, "client_id")),
                ("display_name", _non_empty_str(c, "display_name")),
                ("callback_path", _str(c, "callback_path")),
                ("public_client", _bool(c, "public_client", True)),
                ("default_scopes", _non_empty_strings(c, "default_scopes")),
                ("issue_refresh_token", _bool(c, "issue_refresh_token", True)),
                ("origin_config_key", _str(c, "origin_config_key")),
            ]
        )

    clients = _non_empty_tables(src, "clients", client)
    return _SKIP if clients is _SKIP else {"clients": clients}


def canonical_manifest(m: Table) -> Table:
    """Rebuild a validated manifest the way the Rust ``PluginManifest`` serializes.

    Args:
        m: Manifest previously accepted by ``validate_manifest``.

    Returns:
        The canonical table tree :func:`emit_toml` writes.
    """
    triggers = _table_at(m, "triggers") or {}
    vars_table = _table_at(m, "vars")
    ai = _table_at(m, "ai") or {}

    def module(mod: Table) -> Table:
        return _table(
            [
                ("name", _str(mod, "name")),
                ("path", _non_empty_str(mod, "path")),
                ("type", _str(mod, "type", "js")),
            ]
        )

    def kv(ns: Table) -> Table:
        return _table(
            [
                ("binding", _non_empty_str(ns, "binding")),
                ("namespace", _non_empty_str(ns, "namespace")),
                ("max_value_bytes", _int_unless(ns, "max_value_bytes", 64 * 1024)),
                ("max_keys", _int_unless(ns, "max_keys", 10_000)),
            ]
        )

    return _table(
        [
            ("api_version", _int(m, "api_version", 0)),
            ("id", _str(m, "id")),
            ("name", _opt_str(m, "name")),
            ("version", _opt_str(m, "version")),
            ("logo", _opt_str(m, "logo")),
            ("runtime", _str(m, "runtime", "native")),
            ("command", _opt_str(m, "command")),
            ("args", _non_empty_strings(m, "args")),
            ("entrypoints", _non_empty_strings(m, "entrypoints")),
            ("workerd", _workerd(_table_at(m, "workerd"))),
            ("modules", _non_empty_tables(m, "modules", module)),
            (
                "triggers",
                _non_empty_table(
                    _table(
                        [
                            ("crons", _non_empty_strings(triggers, "crons")),
                            ("jobs", _non_empty_strings(triggers, "jobs")),
                        ]
                    )
                ),
            ),
            ("events", _events(_table_at(m, "events") or {})),
            (
                "databases",
                _non_empty_tables(m, "databases", lambda d: {"binding": _str(d, "binding")}),
            ),
            # `vars` is skipped only when absent: an empty `[vars]` is kept.
            ("vars", _SKIP if vars_table is None else _any_value(vars_table)),
            ("settings", _non_empty_tables(m, "settings", _setting_spec)),
            ("secrets", _named_binding(_table_at(m, "secrets"))),
            ("kv_namespaces", _non_empty_tables(m, "kv_namespaces", kv)),
            ("queues", _queues(_table_at(m, "queues") or {})),
            ("work_fs", _named_binding(_table_at(m, "work_fs"))),
            ("oauth", _named_binding(_table_at(m, "oauth"))),
            ("sockets", _named_binding(_table_at(m, "sockets"))),
            ("webhooks", _non_empty_tables(m, "webhooks", _webhook)),
            ("credentials", _opt_str(m, "credentials")),
            *_media_tables(m),
            (
                "ingress",
                _SKIP
                if _table_at(m, "ingress") is None
                else {"kind": _str(_table_at(m, "ingress") or {}, "kind")},
            ),
            (
                "ai",
                _non_empty_table(
                    _table(
                        [
                            ("serves", _non_empty_strings(ai, "serves")),
                            ("calls", _non_empty_strings(ai, "calls")),
                        ]
                    )
                ),
            ),
            ("capabilities", _capabilities(m)),
            ("cli", _cli(_table_at(m, "cli"))),
            ("oidc", _oidc(_table_at(m, "oidc") or {})),
        ]
    )


def _basic_string(s: str) -> str:
    """Quote ``s`` as a TOML basic string (every SDK writes basic strings).

    Args:
        s: Text.

    Returns:
        Quoted, escaped literal.
    """
    escapes = {
        '"': '\\"',
        "\\": "\\\\",
        "\n": "\\n",
        "\t": "\\t",
        "\r": "\\r",
        "\b": "\\b",
        "\f": "\\f",
    }
    out = ['"']
    for ch in s:
        if ch in escapes:
            out.append(escapes[ch])
        elif ord(ch) < 0x20 or ord(ch) == 0x7F:
            out.append(f"\\u{ord(ch):04X}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def _float(n: float) -> str:
    """Write a float as Rust's ``toml`` does (always a fraction or exponent).

    Args:
        n: Value.

    Returns:
        TOML float literal.
    """
    if math.isnan(n):
        return "nan"
    if math.isinf(n):
        return "inf" if n > 0 else "-inf"
    return repr(n)


def _emit_key(key: str) -> str:
    """Write a TOML key, bare when it can be.

    Args:
        key: Key text.

    Returns:
        Bare or quoted key.
    """
    return key if _BARE_KEY.match(key) else _basic_string(key)


def _is_section(value: Any) -> bool:
    """Whether ``value`` is written as a table or array of tables.

    Args:
        value: Field value.

    Returns:
        ``True`` for a table or a non-empty array of tables.
    """
    if isinstance(value, dict):
        return True
    return isinstance(value, list) and bool(value) and all(isinstance(v, dict) for v in value)


def _emit_inline(value: Any, inline: bool = False) -> str:
    """Write one value in ``key = value`` position.

    Args:
        value: Value (never a section).
        inline: Inside an inline array or table, where arrays stay on one line.

    Returns:
        TOML literal.
    """
    if isinstance(value, str):
        return _basic_string(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return _float(value)
    if isinstance(value, list):
        items = [_emit_inline(item, True) for item in value]
        if not items:
            return "[]"
        if inline:
            return "[" + ", ".join(items) + "]"
        if len(items) == 1:
            return f"[{items[0]}]"
        return "[\n" + "\n".join(f"    {item}," for item in items) + "\n]"
    if isinstance(value, dict):
        cells = [f"{_emit_key(k)} = {_emit_inline(v, True)}" for k, v in value.items()]
        return "{ " + ", ".join(cells) + " }" if cells else "{}"
    # Dates and times: TOML's own text form.
    return value.isoformat() if hasattr(value, "isoformat") else str(value)


def _emit_table(out: list[list[str]], path: list[str], body: Table, header: str | None) -> None:
    """Write one table body (its values, then its sections) into ``out``.

    Args:
        out: Output blocks; each is one section's lines.
        path: Dotted key path of this table (empty for the root).
        body: The table.
        header: Header line, or ``None`` for the root.
    """
    lines: list[str] = []
    sections: list[tuple[str, Any]] = []
    for key, value in body.items():
        if _is_section(value):
            sections.append((key, value))
        else:
            lines.append(f"{_emit_key(key)} = {_emit_inline(value)}")
    # `toml` leaves a table implicit when it holds only sections.
    if header is None:
        out.append(lines)
    elif lines or not sections:
        out.append([header, *lines])
    for key, value in sections:
        child = [*path, _emit_key(key)]
        dotted = ".".join(child)
        if isinstance(value, list):
            for item in value:
                _emit_table(out, child, item, f"[[{dotted}]]")
        else:
            _emit_table(out, child, value, f"[{dotted}]")


def emit_toml(root: Table) -> str:
    """Write a canonical table tree the way ``toml::to_string_pretty`` does.

    Args:
        root: Document root.

    Returns:
        TOML text ending in a newline.
    """
    blocks: list[list[str]] = []
    _emit_table(blocks, [], root, None)
    text = "\n\n".join("\n".join(block) for block in blocks if block)
    return text if text.endswith("\n") else text + "\n"


def format_manifest(m: Table) -> str:
    """Format a validated manifest to canonical TOML.

    Output is byte for byte what the Rust and TypeScript formatters write, so
    ``fmt --check`` agrees across the author tools. Comments are not kept.

    Args:
        m: Manifest previously accepted by ``validate_manifest``.

    Returns:
        Canonical ``plugin.toml`` text ending with a newline.
    """
    return emit_toml(canonical_manifest(m))
