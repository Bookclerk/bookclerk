"""Workerd author runtime — ``BookclerkEntrypoint`` and the named entrypoints.

Python mirror of ``@bookclerk/plugin-sdk/workerd`` (``embed/bookclerk_plugin.js``)
for ``api_version = 3`` guests running as Python Workers:

- ``from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, js``

The default export is a module-level ``Default`` class extending
:class:`BookclerkEntrypoint` with optional ``event(batch)`` / ``job(job)``
triggers. Named entrypoints from ``plugin.toml`` ``entrypoints`` are module-level
classes named after the wire name (``Storefront``, ``Storage``, ``RemoteLibrary``,
``DatabaseAdapter``, ``Cli``, ``Oidc``, ``Metadata``, ``Covers``, ``MetadataFiles``, ``Notifier``) extending the matching ``*Entrypoint``
base. The launcher's adapter isolate reaches them only through the
``bookclerk*`` dispatch methods; authors neither call nor override those.

Inside a Python Workers isolate, ``bookclerk-workerd`` injects this module under
``bookclerk_plugin_sdk.workerd`` — authors do not vendor a relative filepath.
Outside the isolate (authoring / unit tests), lightweight stubs stand in for
``js`` / ``workers`` imports and :func:`js` returns plain Python values.
"""

from __future__ import annotations

import asyncio
import base64
import inspect
import json
import time
from collections.abc import AsyncIterable, AsyncIterator
from dataclasses import dataclass
from datetime import datetime
from typing import TYPE_CHECKING, Any, Protocol

from .limits import (
    LimitExceeded,
    limit_exceeded_details,
    limit_exceeded_message,
    limit_from_details,
    parse_limit_exceeded,
)

if TYPE_CHECKING:
    from bookclerk_plugin_sdk import abi
    from bookclerk_plugin_sdk.abi import DbCapabilities
    from bookclerk_plugin_sdk.db_value import DatabaseBinding, ExecuteReply, ExecuteRequest

try:
    from js import JSON, Object, Response
    from workers import WorkerEntrypoint

    try:
        from pyodide.ffi import to_js as _to_js
    except ImportError:  # pragma: no cover - older Pyodide
        _to_js = None
except ImportError:  # authoring / unit tests outside the isolate
    _to_js = None

    class _JSON:
        @staticmethod
        def parse(s: str):
            return json.loads(s)

    class _Response:
        @staticmethod
        def new(*_a, **_k):
            return None

    class WorkerEntrypoint:  # type: ignore[no-redef]
        """Authoring stub for Cloudflare's ``WorkerEntrypoint`` base class."""

        def __init__(self, *_a, **_k):
            self.env = None

    JSON = _JSON()  # type: ignore[assignment]
    Response = _Response()  # type: ignore[assignment]
    Object = None  # type: ignore[assignment]


# Product constants come from the generated ``_abi`` projection of
# ``schema/plugin.capnp`` — re-exported here for guest convenience.
from ._abi import (  # noqa: E402  (re-export)
    FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_CHANGES,
    FEATURE_STORAGE_CONDITIONAL_WRITE,
    FEATURE_STORAGE_COPY,
    FEATURE_STORAGE_DELETE_MANY,
    FEATURE_STORAGE_HEAD_MANY,
    FEATURE_STORAGE_MOVE,
    FEATURE_STORAGE_MOVE_MANY,
    FEATURE_STREAMS,
    MAX_BATCH_KEYS,
    MAX_CHANGES_CURSOR_BYTES,
    MAX_CHANGES_PAGE,
    MAX_CHECKPOINT_BYTES,
    MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
    MAX_PLUGIN_MIGRATION_TOTAL_OPS,
    MAX_SCALAR_BYTES,
    MAX_SOCKET_ATTACHMENT_BYTES,
    MAX_SOCKET_FRAME_BYTES,
    MAX_STREAM_WINDOW_BYTES,
    PLUGIN_ERROR_CODES,
    PRODUCT_API_VERSION,
)

__all__ = [
    "FEATURE_SCALAR_LIMITS",
    "FEATURE_STORAGE_CHANGES",
    "FEATURE_STORAGE_CONDITIONAL_WRITE",
    "FEATURE_STORAGE_COPY",
    "FEATURE_STORAGE_DELETE_MANY",
    "FEATURE_STORAGE_HEAD_MANY",
    "FEATURE_STORAGE_MOVE",
    "FEATURE_STORAGE_MOVE_MANY",
    "FEATURE_STREAMS",
    "MAX_BATCH_KEYS",
    "MAX_CHANGES_CURSOR_BYTES",
    "MAX_CHANGES_PAGE",
    "MAX_CHECKPOINT_BYTES",
    "MAX_LIST_PAGE",
    "MAX_PLUGIN_MIGRATION_OPS",
    "MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES",
    "MAX_PLUGIN_MIGRATION_TOTAL_OPS",
    "MAX_SCALAR_BYTES",
    "MAX_SOCKET_ATTACHMENT_BYTES",
    "MAX_SOCKET_FRAME_BYTES",
    "MAX_STREAM_WINDOW_BYTES",
    "PRODUCT_API_VERSION",
    "AdapterDatabaseSession",
    "BookclerkEntrypoint",
    "CliEntrypoint",
    "DatabaseAdapterEntrypoint",
    "EventBatch",
    "EventMessage",
    "GuestDatabase",
    "HostWebSocket",
    "JobController",
    "KvNamespace",
    "CoverEntrypoint",
    "MetadataFilesEntrypoint",
    "IngressEntrypoint",
    "MetadataEntrypoint",
    "NamedEntrypoint",
    "NotifierEntrypoint",
    "NotifyBinding",
    "OidcEntrypoint",
    "PluginError",
    "QueueBatch",
    "QueueBinding",
    "QueueMessage",
    "RemoteLibraryEntrypoint",
    "ScheduledController",
    "SocketsBinding",
    "StorageEntrypoint",
    "StorefrontEntrypoint",
    "cli_args",
    "decode_extensible_config",
    "decode_socket_attachment",
    "encode_socket_attachment",
    "event_batch_results",
    "granted_databases",
    "job_outcome_for",
    "js",
    "json_payload",
    "py",
    "queue_batch_results",
    "socket_message_data",
    "transport_database",
    "transport_kv",
    "transport_queue",
    "transport_sockets",
]


# ---------------------------------------------------------------------------
# JS <-> Python value helpers
# ---------------------------------------------------------------------------


def js(value: Any) -> Any:
    """Convert a Python value to a JS value for Workers RPC results.

    Inside the isolate this uses ``pyodide.ffi.to_js`` with plain-object
    dictionaries (``bytes`` become ``Uint8Array``); when ``to_js`` is not
    available it falls back to ``JSON.parse(json.dumps(value))``. Under the
    authoring stubs the value is returned unchanged.

    Args:
        value: JSON-serializable Python value (``bytes`` allowed for ``Data``).

    Returns:
        The JS value (or the original Python value under the authoring stubs).
    """
    if _to_js is not None and Object is not None:
        return _to_js(value, dict_converter=Object.fromEntries)
    if Object is None:
        return value
    return JSON.parse(json.dumps(value))


def _plain_bytes(value: Any) -> Any:
    """Replace ``memoryview`` values with ``bytes`` at every nesting depth.

    Pyodide's ``to_py()`` turns a ``Uint8Array`` into a ``memoryview``, also
    inside converted objects and arrays, so a ``Data`` field such as
    ``MetadataFileParseParams["bytes"]`` would otherwise reach a handler as a
    ``memoryview`` rather than the declared ``bytes``.

    Args:
        value: A value ``to_py()`` returned.

    Returns:
        The same value with every ``memoryview`` copied into ``bytes``.
    """
    if isinstance(value, memoryview):
        return bytes(value)
    if isinstance(value, dict):
        return {key: _plain_bytes(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_plain_bytes(item) for item in value]
    return value


def py(value: Any) -> Any:
    """Convert a JS value delivered over Workers RPC into plain Python.

    ``JsProxy`` objects are converted with ``to_py()`` (``Uint8Array`` →
    ``bytes`` at any depth); Python values pass through unchanged.

    Args:
        value: A JsProxy or Python value.

    Returns:
        A plain Python value (``dict`` / ``list`` / ``bytes`` / scalars).
    """
    if value is None or isinstance(value, (str, int, float, bool, bytes, dict, list)):
        return value
    to_py = getattr(value, "to_py", None)
    if callable(to_py):
        return _plain_bytes(to_py())
    if isinstance(value, (bytearray, memoryview)):
        return bytes(value)
    return value


def _get(obj: Any, key: str, default: Any = None) -> Any:
    """Read ``key`` from a mapping or attribute-bearing object."""
    if obj is None:
        return default
    if isinstance(obj, dict):
        val = obj.get(key)
        return default if val is None else val
    val = getattr(obj, key, None)
    return default if val is None else val


def _bytes(value: Any) -> bytes:
    """Coerce a JS ``Uint8Array`` / Python buffer into ``bytes``."""
    value = py(value)
    if isinstance(value, bytes):
        return value
    if isinstance(value, (bytearray, memoryview)):
        return bytes(value)
    if isinstance(value, list) and all(isinstance(b, int) for b in value):
        return bytes(value)
    return b""


def _utf8_len(text: str) -> int:
    return len(text.encode("utf-8"))


def _error_message(err: BaseException) -> str:
    return str(err) or err.__class__.__name__


def _await_maybe(value: Any) -> Any:
    """Return an awaitable for ``value`` whether or not the author used ``async``."""

    async def _wrap() -> Any:
        if inspect.isawaitable(value):
            return await value
        return value

    return _wrap()


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


def _parse_details(text: str) -> dict[str, Any] | None:
    """Parse the wire ``details`` JSON object text; anything else is dropped.

    Text past ``MAX_SCALAR_BYTES`` (the ABI scalar ceiling, in UTF-8 bytes)
    is dropped too.
    """
    if not text:
        return None
    try:
        if len(text.encode("utf-8")) > MAX_SCALAR_BYTES:
            return None
    except UnicodeEncodeError:
        return None
    try:
        parsed = json.loads(text)
    except ValueError:
        return None
    return parsed if isinstance(parsed, dict) else None


class PluginError(RuntimeError):
    """SDK-thrown failure. Unknown wire codes stay on ``wire_code``.

    Attributes:
        code: Known ``PluginErrorCode`` wire string, or ``unknown``.
        wire_code: Raw wire code, including codes this SDK does not know.
        details: Structured detail object from the wire (``details`` JSON),
            or ``None`` when the error carries none.
    """

    def __init__(
        self,
        code: str,
        message: str,
        details: dict[str, Any] | str | None = None,
    ):
        super().__init__(message)
        self.wire_code = code
        self.code = code if code in PLUGIN_ERROR_CODES else "unknown"
        self.details: dict[str, Any] | None = (
            _parse_details(details) if isinstance(details, str) else details
        )

    @classmethod
    def from_wire(cls, code: str, message: str, details: str = "") -> PluginError:
        """Build a ``PluginError`` from a wire ``code`` string.

        Args:
            code: Snake_case wire code (unknown codes are kept on ``wire_code``).
            message: Operator-facing error text.
            details: ``details`` JSON object text from the wire (empty when none).

        Returns:
            A ``PluginError`` whose ``code`` is a known variant or ``unknown``.
        """
        return cls(code, message, details)

    @classmethod
    def limit_exceeded(cls, limit: LimitExceeded) -> PluginError:
        """Build a ``limit_exceeded`` error carrying ``limit`` in message and details.

        Args:
            limit: Exceeded cap.

        Returns:
            A ``PluginError`` with code ``limit_exceeded``.
        """
        return cls("limit_exceeded", limit_exceeded_message(limit), limit_exceeded_details(limit))

    @property
    def limit(self) -> LimitExceeded | None:
        """The exceeded cap when this is a ``limit_exceeded`` error.

        Returns:
            The cap from ``details`` first, then from the canonical message;
            ``None`` for every other code.
        """
        if self.wire_code != "limit_exceeded":
            return None
        return limit_from_details(self.details) or parse_limit_exceeded(str(self))

    @property
    def details_text(self) -> str:
        """``details`` as the JSON text the wire carries.

        Returns:
            JSON object text, or an empty string when the error has none.
        """
        return "" if self.details is None else json.dumps(self.details, separators=(",", ":"))


class WriteChunksError(PluginError):
    """A :meth:`DatabaseBinding.write_chunks` run that failed part way.

    ``code``, ``message`` and ``details`` are the failed batch's.

    Attributes:
        next: Statements committed before the failure; resume with
            ``start=next``. The failed batch may have committed with its reply
            lost: resuming resends it under the same operation id, so the host
            replays its receipt instead of writing it twice.
    """

    def __init__(self, next_offset: int, cause: PluginError) -> None:
        super().__init__(cause.wire_code, str(cause), cause.details)
        self.next = next_offset


def _unsupported(method: str) -> PluginError:
    """Build the ``unsupported`` error the entrypoint base classes raise.

    Args:
        method: Wire method name.

    Returns:
        A ``PluginError`` with ``code="unsupported"``.
    """
    return PluginError.from_wire("unsupported", f"{method} not implemented")


# ---------------------------------------------------------------------------
# Bindings on ``env``
# ---------------------------------------------------------------------------


def decode_extensible_config(cfg: Any) -> Any:
    """Decode an ``ExtensibleConfig`` into the plain value on ``env.CONFIG``.

    Args:
        cfg: ``{"schemaVersion", "mediaType", "payload"}`` (JS or Python).

    Returns:
        The parsed JSON object for JSON media types (``{}`` when empty or
        malformed); ``{"mediaType", "text"}`` for other media types.
    """
    cfg = py(cfg)
    if cfg is None:
        return {}
    if not isinstance(cfg, dict):
        return cfg
    payload = cfg.get("payload")
    if isinstance(payload, dict) and "schemaVersion" not in cfg:
        return payload
    text = payload if isinstance(payload, str) else _bytes(payload).decode("utf-8", "replace")
    media_type = str(cfg.get("mediaType") or "")
    if not text.strip():
        return {}
    if media_type == "" or "json" in media_type.lower():
        try:
            parsed = json.loads(text)
        except ValueError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {"mediaType": media_type, "text": text}


def json_payload(value: Any) -> dict[str, Any]:
    """Wrap a JSON-serializable value as an ``application/json`` ``ExtensibleConfig``.

    This is the shape of ``CliInvokeResult.payload`` and every other extensible
    payload on the wire (schema version 1).

    Args:
        value: JSON-serializable Python value.

    Returns:
        ``{"schemaVersion": 1, "mediaType": "application/json", "payload": bytes}``.
    """
    return {
        "schemaVersion": 1,
        "mediaType": "application/json",
        "payload": json.dumps(value).encode("utf-8"),
    }


def cli_args(params: Any) -> dict[str, str]:
    """Turn ``CliInvokeParams.args`` (``[{name, value}]``) into a ``{name: value}`` dict.

    Args:
        params: ``CliInvokeParams`` (JS or Python).

    Returns:
        Argument values keyed by name.
    """
    params = py(params)
    out: dict[str, str] = {}
    for arg in _get(params, "args", []) or []:
        arg = py(arg)
        name = _get(arg, "name")
        if isinstance(name, str):
            out[name] = str(_get(arg, "value", ""))
    return out


class _EnvView:
    """Per-invocation ``env``: granted bindings layered over the isolate's static env."""

    def __init__(self, raw: Any, granted: dict[str, Any]) -> None:
        self._raw = raw
        self._granted = granted

    def __getattr__(self, name: str) -> Any:
        granted = self.__dict__.get("_granted") or {}
        if name in granted:
            return granted[name]
        raw = self.__dict__.get("_raw")
        if raw is None:
            raise AttributeError(name)
        return getattr(raw, name)

    def __getitem__(self, name: str) -> Any:
        try:
            return self.__getattr__(name)
        except AttributeError as err:
            raise KeyError(name) from err

    def get(self, name: str, default: Any = None) -> Any:
        """Read a binding, returning ``default`` when it is not granted."""
        try:
            return self.__getattr__(name)
        except AttributeError:
            return default


def _network_grant(network: Any) -> dict[str, Any]:
    """The ``env.NETWORK`` dict: the grant in effect, with every list present.

    Args:
        network: The bridge context's ``network`` value.

    Returns:
        ``outbound``, ``domains``, ``tcp`` and ``settings`` (each setting a
        dict with ``key``, ``network``, ``state`` and, when set,
        ``destination`` and ``error``).
    """

    def as_list(value: Any) -> list[Any]:
        return [py(item) for item in py(value)] if value is not None else []

    return {
        "outbound": _get(network, "outbound") is True,
        "domains": as_list(_get(network, "domains")),
        "tcp": as_list(_get(network, "tcp")),
        "settings": as_list(_get(network, "settings")),
    }


def _invocation_env(raw_env: Any, context: Any) -> _EnvView:
    granted: dict[str, Any] = {}
    if isinstance(raw_env, _EnvView):
        granted.update(raw_env._granted)
        raw_env = raw_env._raw
    ctx = context if context is not None else {}
    config = _get(ctx, "config")
    if config is not None:
        granted["CONFIG"] = decode_extensible_config(config)
    secrets = _get(ctx, "secrets")
    if secrets is not None:
        granted["SECRETS"] = decode_extensible_config(secrets)
    network = _get(ctx, "network")
    if network is not None:
        granted["NETWORK"] = _network_grant(network)
    storage = _get(ctx, "storage")
    if storage is not None:
        granted["WORK_FS"] = storage
    events = _get(ctx, "events")
    if events is not None:
        granted["EVENTS"] = events
    notify = _get(ctx, "notify")
    if notify is not None:
        granted["NOTIFY"] = transport_notify(notify)
    databases = _get(ctx, "databases")
    if databases is not None:
        for entry in databases:
            name = _get(entry, "name")
            if not isinstance(name, str) or not name:
                continue
            database = _get(entry, "database")
            if database is not None:
                granted[name] = database
                continue
            transport = _get(entry, "transport")
            if transport is not None:
                granted[name] = transport_database(transport)
    queues = _get(ctx, "queues")
    if queues is not None:
        for entry in queues:
            name = _get(entry, "name")
            queue = _get(entry, "queue")
            if not isinstance(name, str) or not name or queue is None:
                continue
            granted[name] = transport_queue(queue)
    kv = _get(ctx, "kv")
    if kv is not None:
        for entry in kv:
            name = _get(entry, "name")
            namespace = _get(entry, "kv")
            if not isinstance(name, str) or not name or namespace is None:
                continue
            granted[name] = transport_kv(namespace)
    sockets = _sockets_stub(ctx)
    if sockets is not None:
        granted[sockets[0]] = transport_sockets(sockets[1])
    return _EnvView(raw_env, granted)


def _apply_invocation_env(instance: Any, context: Any) -> None:
    instance.env = _invocation_env(getattr(instance, "env", None), context)


def _invocation_of(context: Any) -> dict[str, Any]:
    """Invocation envelope (``Invocation`` struct) with zero values normalized."""
    inv = _get(context, "invocation", {}) or {}
    return {
        "id": str(_get(inv, "id", "")),
        "accountId": str(_get(inv, "accountId", "")),
        "deadlineUnixMs": int(_get(inv, "deadlineUnixMs", 0) or 0),
        "correlationId": str(_get(inv, "correlationId", "")),
        "causationId": str(_get(inv, "causationId", "")),
    }


# ---------------------------------------------------------------------------
# Event trigger: ``event(batch)`` shaped like Workers ``queue(batch)``
# ---------------------------------------------------------------------------


def _unix_ms(value: Any) -> int:
    if value is None:
        return 0
    if isinstance(value, datetime):
        return int(value.timestamp() * 1000)
    try:
        n = float(value)
    except (TypeError, ValueError):
        return 0
    return int(n) if n > 0 else 0


def _checkpoint_text(checkpoint: Any) -> str:
    if checkpoint is None:
        return ""
    text = checkpoint if isinstance(checkpoint, str) else json.dumps(checkpoint)
    size = _utf8_len(text)
    if size > MAX_CHECKPOINT_BYTES:
        raise PluginError.from_wire(
            "payload_too_large",
            f"checkpoint is {size} bytes; exceeds maxCheckpointBytes ({MAX_CHECKPOINT_BYTES})",
        )
    return text


class EventMessage:
    """One delivered domain event.

    Exactly one outcome is recorded per message; the first of ``ack`` /
    ``retry`` / ``reject`` / ``dead_letter`` / ``suspend`` wins and later calls
    are ignored (as with Workers queue messages). Messages left untouched are
    acked when ``event()`` returns and retried when it raises.
    """

    def __init__(self, event: Any) -> None:
        e = py(event) or {}
        self.id: str = str(_get(e, "eventId", ""))
        self.type: str = str(_get(e, "eventType", ""))
        self.schema_version: int = int(_get(e, "schemaVersion", 0) or 0)
        self.timestamp_unix_ms: int = int(_get(e, "occurredAtUnixMs", 0) or 0)
        self.attempts: int = int(_get(e, "deliveryAttempt", 1) or 1)
        self.account_id: str = str(_get(e, "accountId", ""))
        self.source: str = str(_get(e, "source", ""))
        self.correlation_id: str = str(_get(e, "correlationId", ""))
        self.causation_id: str = str(_get(e, "causationId", ""))
        self.deduplication_key: str = str(_get(e, "deduplicationKey", ""))
        self.body: bytes = _bytes(_get(e, "payload"))
        self.invocation_sequence: int = int(_get(e, "invocationSequence", 0) or 0)
        self.resume_pending: bool = bool(_get(e, "resumePending", False))
        checkpoint_json = str(_get(e, "checkpointJson", ""))
        self.checkpoint: dict[str, Any] | None = (
            {
                "json": checkpoint_json,
                "schemaVersion": int(_get(e, "checkpointSchemaVersion", 0) or 0),
            }
            if checkpoint_json
            else None
        )
        self.raw: Any = e
        self._result: dict[str, Any] | None = None

    def json(self) -> Any:
        """Decode ``body`` as JSON (``{}`` for an empty body).

        Returns:
            The parsed payload.
        """
        if not self.body:
            return {}
        return json.loads(self.body.decode("utf-8"))

    @property
    def result(self) -> dict[str, Any] | None:
        """Recorded wire ``EventResult`` (flat ``{"kind": ...}``), or ``None`` while undecided."""
        return self._result

    def _record(self, result: dict[str, Any]) -> None:
        if self._result is None:
            self._result = result

    def ack(self) -> None:
        """Mark handled; the host marks the event delivered."""
        self._record({"kind": "ack"})

    def retry(
        self,
        *,
        retry_at: datetime | int | float | None = None,
        delay_seconds: float = 0,
        reason: str = "",
    ) -> None:
        """Redeliver later.

        Args:
            retry_at: Explicit earliest redelivery instant (``datetime`` or Unix ms).
            delay_seconds: Delay from now; ignored when ``retry_at`` is set.
            reason: Short operator-facing reason.
        """
        explicit = _unix_ms(retry_at)
        delay = float(delay_seconds or 0)
        self._record(
            {
                "kind": "retry",
                "retryAtUnixMs": explicit
                or (int(time.time() * 1000 + delay * 1000) if delay > 0 else 0),
                "reason": str(reason or ""),
            }
        )

    def reject(self, reason: str = "") -> None:
        """Stop delivering; the host records ``reason``.

        Args:
            reason: Short operator-facing reason.
        """
        self._record({"kind": "reject", "reason": str(reason or "")})

    def dead_letter(self, reason: str = "") -> None:
        """Park for operator review.

        Args:
            reason: Short operator-facing reason.
        """
        self._record({"kind": "deadLetter", "reason": str(reason or "")})

    def suspend(
        self,
        *,
        checkpoint: Any = None,
        checkpoint_schema_version: int = 1,
        wake_at: datetime | int | float | None = None,
        wake_on_event_type: str = "",
        wake_on_filter: Any = None,
    ) -> None:
        """Release with a bounded checkpoint; the host redelivers later.

        Args:
            checkpoint: JSON-serializable checkpoint (at most ``MAX_CHECKPOINT_BYTES``).
            checkpoint_schema_version: Schema version of ``checkpoint``.
            wake_at: Earliest resume instant (``datetime`` or Unix ms).
            wake_on_event_type: Also wake when an event of this type arrives.
            wake_on_filter: Host-owned payload filter for wake events.

        Raises:
            PluginError: ``payload_too_large`` when the checkpoint exceeds the cap.
        """
        if wake_on_filter is None:
            filter_json = ""
        elif isinstance(wake_on_filter, str):
            filter_json = wake_on_filter
        else:
            filter_json = json.dumps(wake_on_filter)
        self._record(
            {
                "kind": "suspended",
                "checkpointJson": _checkpoint_text(checkpoint),
                "checkpointSchemaVersion": int(checkpoint_schema_version or 1),
                "wakeAtUnixMs": _unix_ms(wake_at),
                "wakeOnEventType": str(wake_on_event_type or ""),
                "wakeOnFilterJson": filter_json,
            }
        )


class EventBatch:
    """One ``event(batch)`` delivery (``EventBatch`` struct), Workers ``MessageBatch``-shaped."""

    def __init__(self, events: Any, invocation: dict[str, Any] | None = None) -> None:
        raw = py(events) or []
        self.messages: tuple[EventMessage, ...] = tuple(EventMessage(e) for e in raw)
        self.invocation: dict[str, Any] = invocation or {}

    @property
    def type(self) -> str:
        """Event type shared by the batch (``""`` when mixed or empty)."""
        if not self.messages:
            return ""
        first = self.messages[0].type
        return first if all(m.type == first for m in self.messages) else ""

    def ack_all(self) -> None:
        """Acknowledge every message."""
        for m in self.messages:
            m.ack()

    def retry_all(self, **options: Any) -> None:
        """Retry every message with the same :meth:`EventMessage.retry` options.

        Args:
            **options: Keyword arguments forwarded to :meth:`EventMessage.retry`.
        """
        for m in self.messages:
            m.retry(**options)


def event_batch_results(batch: EventBatch, failed: BaseException | None) -> list[dict[str, Any]]:
    """Translate recorded outcomes into the wire ``EventResult`` list.

    Args:
        batch: The delivered batch.
        failed: Exception raised by the author's ``event()``, if any; undecided
            messages then retry instead of ack.

    Returns:
        One flat ``{"kind": ...}`` result per message, in delivery order.
    """
    out: list[dict[str, Any]] = []
    for m in batch.messages:
        if m.result is not None:
            out.append(m.result)
        elif failed is not None:
            out.append({"kind": "retry", "retryAtUnixMs": 0, "reason": _error_message(failed)})
        else:
            out.append({"kind": "ack"})
    return out


# ---------------------------------------------------------------------------
# Queue trigger: ``queue(batch)`` shaped like Workers ``queue(batch)``
# ---------------------------------------------------------------------------


class QueueMessage:
    """One delivered queue message (``QueueMessage`` struct), Workers ``Message``-shaped.

    Exactly one outcome is recorded per message; the first of ``ack`` /
    ``retry`` wins and later calls are ignored. Messages left untouched are
    acked when ``queue()`` returns and retried when it raises.
    """

    def __init__(self, message: Any) -> None:
        m = py(message) or {}
        self.id: str = str(_get(m, "id", ""))
        self.timestamp_unix_ms: int = int(_get(m, "timestampUnixMs", 0) or 0)
        self.body: bytes = _bytes(_get(m, "body"))
        self.content_type: str = str(_get(m, "contentType", "") or "") or "json"
        self.attempts: int = int(_get(m, "attempts", 1) or 1)
        self.source: str = str(_get(m, "source", ""))
        self.raw: Any = m
        self._result: dict[str, Any] | None = None

    def json(self) -> Any:
        """Decode ``body`` as JSON (``None`` for an empty body).

        Returns:
            The parsed body.
        """
        if not self.body:
            return None
        return json.loads(self.body.decode("utf-8"))

    def text(self) -> str:
        """Decode ``body`` as UTF-8 text.

        Returns:
            The body text.
        """
        return self.body.decode("utf-8")

    @property
    def result(self) -> dict[str, Any] | None:
        """Recorded outcome (flat ``{"kind": ...}``), or ``None`` while undecided."""
        return self._result

    def _record(self, result: dict[str, Any]) -> None:
        if self._result is None:
            self._result = result

    def ack(self) -> None:
        """Mark handled; the host marks the message delivered."""
        self._record({"kind": "ack"})

    def retry(self, *, delay_seconds: float = 0, reason: str = "") -> None:
        """Redeliver later.

        Args:
            delay_seconds: Seconds before redelivery; ``0`` applies the
                consumer's ``retry_delay``.
            reason: Short operator-facing reason.
        """
        self._record(
            {
                "kind": "retry",
                "delaySeconds": max(0, int(delay_seconds or 0)),
                "reason": str(reason or ""),
            }
        )


class QueueBatch:
    """One ``queue(batch)`` delivery (``QueueBatch`` struct), Workers ``MessageBatch``-shaped."""

    def __init__(self, queue: Any, messages: Any, invocation: dict[str, Any] | None = None) -> None:
        self.queue: str = str(queue or "")
        raw = py(messages) or []
        self.messages: tuple[QueueMessage, ...] = tuple(QueueMessage(m) for m in raw)
        self.invocation: dict[str, Any] = invocation or {}

    def ack_all(self) -> None:
        """Acknowledge every message."""
        for m in self.messages:
            m.ack()

    def retry_all(self, **options: Any) -> None:
        """Retry every message with the same :meth:`QueueMessage.retry` options.

        Args:
            **options: Keyword arguments forwarded to :meth:`QueueMessage.retry`.
        """
        for m in self.messages:
            m.retry(**options)


def queue_batch_results(batch: QueueBatch, failed: BaseException | None) -> list[dict[str, Any]]:
    """Translate recorded outcomes into the wire ``QueueResult`` list.

    Args:
        batch: The delivered batch.
        failed: Exception raised by the author's ``queue()``, if any; undecided
            messages then retry instead of ack.

    Returns:
        One flat ``{"kind": ...}`` result per message, in delivery order.
    """
    out: list[dict[str, Any]] = []
    for m in batch.messages:
        if m.result is not None:
            out.append(m.result)
        elif failed is not None:
            out.append({"kind": "retry", "delaySeconds": 0, "reason": _error_message(failed)})
        else:
            out.append({"kind": "ack"})
    return out


class QueueBinding:
    """``[[queues.producers]]`` binding (``env.<BINDING>``): Workers ``Queue``-shaped producer.

    The adapter isolate owns the queue grant and serves ``send`` over the
    granted channel; this wrapper converts Python bodies into the JS values
    the adapter stub expects.
    """

    def __init__(self, stub: Any) -> None:
        self._stub = stub

    async def send(
        self,
        body: Any,
        *,
        content_type: str = "json",
        delay_seconds: int = 0,
    ) -> dict[str, Any]:
        """Send one message.

        Args:
            body: JSON value (default), ``str`` for ``text``, or ``bytes`` for ``bytes``.
            content_type: ``json``, ``text``, or ``bytes``.
            delay_seconds: Seconds before the message may be delivered.

        Returns:
            ``{"messageIds": [...]}`` with the outbox id the host assigned.
        """
        return await self.send_batch(
            [{"body": body, "content_type": content_type}], delay_seconds=delay_seconds
        )

    async def send_batch(
        self, messages: list[dict[str, Any]], *, delay_seconds: int = 0
    ) -> dict[str, Any]:
        """Send up to ``MAX_LIST_PAGE`` messages in order.

        Args:
            messages: ``{"body": ..., "content_type": ..., "delay_seconds": ...}`` entries.
            delay_seconds: Default delay for entries that carry none.

        Returns:
            ``{"messageIds": [...]}`` with the outbox ids in send order.
        """
        entries = []
        for message in messages or []:
            entries.append(
                {
                    "body": _get(message, "body"),
                    "contentType": str(
                        _get(message, "content_type") or _get(message, "contentType") or "json"
                    ),
                    "delaySeconds": int(
                        _get(message, "delay_seconds") or _get(message, "delaySeconds") or 0
                    ),
                }
            )
        reply = await self._stub.sendBatch(
            js(entries), js({"delaySeconds": int(delay_seconds or 0)})
        )
        out = py(reply) or {}
        return {"messageIds": [str(i) for i in (_get(out, "messageIds") or [])]}


def transport_queue(stub: Any) -> QueueBinding:
    """Wrap the adapter's granted queue stub as a :class:`QueueBinding`.

    Args:
        stub: ``QueueBinding``-shaped JS stub from the granted context.

    Returns:
        The author-facing binding.
    """
    return QueueBinding(stub)


class NotifyBinding:
    """``NOTIFY`` binding, present when ``plugin.toml`` declares ``[[notifications.types]]``.

    The host routes each raise by the administrators' rules and each
    patron's preferences; the plugin never learns where it went. The adapter
    isolate owns the notify grant; this wrapper converts Python values into
    the JS values the adapter stub expects.
    """

    def __init__(self, stub: Any) -> None:
        self._stub = stub

    async def raise_(
        self,
        notification_type: str,
        *,
        fields: list[abi.NotificationField] | dict[str, str] | None = None,
        patron_id: str = "",
        link_path: str = "",
        group_key: str = "",
        alert_action: abi.NotificationAlertAction = "none",
        alert_key: str = "",
    ) -> abi.NotifyRaised:
        """Raise one declared notification type (``env.NOTIFY.raise``).

        Args:
            notification_type: Type id from ``[[notifications.types]]``,
                without a plugin prefix.
            fields: Labelled values as ``{"name", "value"}`` rows or a plain
                ``name -> value`` dict (at most ``MAX_NOTIFICATION_FIELDS``).
            patron_id: Patron the notification is for; required for a
                ``patron`` type and must be inside the invocation's account scope.
            link_path: Path inside the library to link to (``/books/42``).
            group_key: Notifications with one group key may be threaded together.
            alert_action: ``none``, ``trigger`` or ``resolve``.
            alert_key: Alert to open or close; required unless ``alert_action``
                is ``none``.

        Returns:
            ``{"notificationId": ...}`` the host assigned.

        Raises:
            PluginError: ``invalid_params`` for an undeclared type or a
                malformed raise, ``forbidden`` for a patron outside the account
                scope, ``rate_limited`` past the per-instance raise budget.
        """
        if not isinstance(notification_type, str) or not notification_type:
            raise PluginError("invalid_params", "raise requires notification_type")
        if fields is None:
            rows: list[dict[str, str]] = []
        elif isinstance(fields, dict):
            rows = [{"name": str(k), "value": str(v)} for k, v in fields.items()]
        else:
            rows = [
                {"name": str(_get(f, "name", "")), "value": str(_get(f, "value", ""))}
                for f in fields
            ]
        raise_params: dict[str, Any] = {
            "notificationType": notification_type,
            "fields": rows,
            "alertAction": alert_action or "none",
        }
        if patron_id:
            raise_params["patronId"] = patron_id
        if link_path:
            raise_params["linkPath"] = link_path
        if group_key:
            raise_params["groupKey"] = group_key
        if alert_key:
            raise_params["alertKey"] = alert_key
        reply = py(await getattr(self._stub, "raise")(js(raise_params))) or {}
        return {"notificationId": str(_get(reply, "notificationId", ""))}


def transport_notify(stub: Any) -> NotifyBinding:
    """Wrap the adapter's granted notify stub as a :class:`NotifyBinding`.

    Args:
        stub: ``NotifyBinding``-shaped JS stub from the granted context.

    Returns:
        The author-facing binding.
    """
    return NotifyBinding(stub)


class KvNamespace:
    """``[[kv_namespaces]]`` binding (``env.<BINDING>``): Workers ``KVNamespace``-shaped storage.

    The host serves the namespace from the library database, scoped to this
    plugin and the namespace the row names, so values follow the instance
    across hosts. The adapter isolate owns the kv grant; this wrapper converts
    Python values into the JS values the adapter stub expects.
    """

    def __init__(self, stub: Any) -> None:
        self._stub = stub

    async def get(self, key: str, type: str = "text") -> Any:
        """Read one key.

        Args:
            key: Key, at most ``MAX_KV_KEY_BYTES`` UTF-8 bytes.
            type: ``text`` (``str``), ``json`` (decoded value), or ``bytes``.

        Returns:
            The value, or ``None`` when absent or expired.
        """
        value, _ = await self.get_with_metadata(key, type)
        return value

    async def get_with_metadata(self, key: str, type: str = "text") -> tuple[Any, Any]:
        """Read one key with its metadata.

        Args:
            key: Key to read.
            type: ``text``, ``json``, or ``bytes``.

        Returns:
            ``(value, metadata)``; both ``None`` when absent.
        """
        js_type = "arrayBuffer" if type == "bytes" else type
        if js_type not in ("text", "json", "arrayBuffer"):
            raise ValueError(f"unknown kv value type `{type}`")
        reply = py(await self._stub.getWithMetadata(key, js_type)) or {}
        value = _get(reply, "value")
        if js_type == "arrayBuffer" and value is not None and not isinstance(value, bytes):
            value = bytes(py(value) or b"")
        return value, _get(reply, "metadata")

    async def put(
        self,
        key: str,
        value: str | bytes,
        *,
        expiration: int | None = None,
        expiration_ttl: int | None = None,
        metadata: Any = None,
    ) -> None:
        """Write one key, replacing any previous value and metadata.

        Args:
            key: Key to write.
            value: ``str`` (stored as UTF-8) or ``bytes``.
            expiration: Absolute expiry in seconds since the Unix epoch.
            expiration_ttl: Relative expiry in seconds from the write.
            metadata: JSON-serializable metadata.
        """
        options: dict[str, Any] = {}
        if expiration is not None:
            options["expiration"] = int(expiration)
        if expiration_ttl is not None:
            options["expirationTtl"] = int(expiration_ttl)
        if metadata is not None:
            options["metadata"] = metadata
        await self._stub.put(key, js(value), js(options))

    async def delete(self, key: str) -> None:
        """Delete one key; a missing key is not an error."""
        await self._stub.delete(key)

    async def list(self, *, prefix: str = "", cursor: str = "", limit: int = 0) -> dict[str, Any]:
        """List keys in lexical order, one page at a time.

        Args:
            prefix: Only keys starting with this prefix.
            cursor: Cursor from a previous page.
            limit: Most keys to return (host default when 0).

        Returns:
            ``{"keys": [{"name", "expiration"?, "metadata"?}], "list_complete": bool, "cursor"?: str}``.
        """
        options: dict[str, Any] = {}
        if prefix:
            options["prefix"] = prefix
        if cursor:
            options["cursor"] = cursor
        if limit:
            options["limit"] = int(limit)
        reply = py(await self._stub.list(js(options))) or {}
        return dict(reply) if isinstance(reply, dict) else {"keys": [], "list_complete": True}


def transport_kv(stub: Any) -> KvNamespace:
    """Wrap the adapter's granted KV stub as a :class:`KvNamespace`.

    Args:
        stub: ``KvNamespaceBinding``-shaped JS stub from the granted context.

    Returns:
        The author-facing binding.
    """
    return KvNamespace(stub)


# ---------------------------------------------------------------------------
# ``[sockets]``: host-brokered WebSockets shaped like hibernatable Workers sockets
# ---------------------------------------------------------------------------


def _b64(data: bytes) -> str:
    """Standard base64 text of ``data``."""
    return base64.b64encode(data).decode("ascii")


def _from_b64(text: Any) -> bytes:
    """Decode standard base64; anything that is not a non-empty string is empty."""
    if not isinstance(text, str) or not text:
        return b""
    return base64.b64decode(text)


def _route_frame(data: str | bytes | bytearray | memoryview) -> dict[str, str]:
    """Route JSON form of one socket message, checked against ``MAX_SOCKET_FRAME_BYTES``.

    Args:
        data: ``str`` for a text message, bytes-like for a binary one.

    Returns:
        ``{"text": ...}`` or ``{"binary": <base64>}``.

    Raises:
        PluginError: ``invalid_params`` for another type, ``payload_too_large``
            for a message over ``MAX_SOCKET_FRAME_BYTES`` bytes.
    """
    if isinstance(data, str):
        size = _utf8_len(data)
        raw: bytes | None = None
    elif isinstance(data, (bytes, bytearray, memoryview)):
        raw = bytes(data)
        size = len(raw)
    else:
        raise PluginError.from_wire("invalid_params", "a socket message must be str or bytes")
    if size > MAX_SOCKET_FRAME_BYTES:
        raise PluginError.from_wire(
            "payload_too_large",
            f"socket message of {size} bytes exceeds {MAX_SOCKET_FRAME_BYTES}",
        )
    return {"binary": _b64(raw)} if raw is not None else {"text": data}  # type: ignore[dict-item]


def encode_socket_attachment(value: Any) -> bytes:
    """Encode a socket attachment as the host stores it: UTF-8 JSON.

    Args:
        value: JSON-serializable value; ``None`` stores no bytes.

    Returns:
        At most ``MAX_SOCKET_ATTACHMENT_BYTES`` bytes.

    Raises:
        PluginError: ``invalid_params`` when JSON cannot encode the value,
            ``payload_too_large`` when it does not fit.
    """
    if value is None:
        return b""
    try:
        text = json.dumps(value, separators=(",", ":"))
    except (TypeError, ValueError) as err:
        raise PluginError.from_wire(
            "invalid_params", f"socket attachment is not JSON: {err}"
        ) from err
    data = text.encode("utf-8")
    if len(data) > MAX_SOCKET_ATTACHMENT_BYTES:
        raise PluginError.from_wire(
            "payload_too_large",
            f"socket attachment of {len(data)} bytes exceeds {MAX_SOCKET_ATTACHMENT_BYTES}",
        )
    return data


def decode_socket_attachment(data: bytes) -> Any:
    """Decode stored socket attachment bytes.

    Args:
        data: Bytes the host kept.

    Returns:
        The JSON value, ``None`` when empty, or the text itself when the bytes
        are not JSON (an attachment another SDK stored).
    """
    if not data:
        return None
    text = data.decode("utf-8", errors="replace")
    try:
        return json.loads(text)
    except ValueError:
        return text


def socket_message_asks_poll(returned: Any) -> bool:
    """Whether a ``web_socket_message`` return value asks the host to poll now.

    Args:
        returned: What the handler returned: ``None``, or a mapping with
            ``poll_events`` / ``pollEvents``.

    Returns:
        ``True`` only for an explicit ``True`` under either key.
    """
    if not isinstance(returned, dict):
        return False
    return returned.get("poll_events") is True or returned.get("pollEvents") is True


def socket_message_data(frame: Any) -> str | bytes:
    """The message a handler receives for one wire frame.

    Args:
        frame: Decoded wire ``SocketFrame`` (``{"kind", "value"}``).

    Returns:
        ``str`` for a text frame, ``bytes`` for a binary one.
    """
    frame = py(frame)
    if _get(frame, "kind") == "binary":
        return _bytes(_get(frame, "value"))
    value = _get(frame, "value")
    return value if isinstance(value, str) else ""


async def _socket_call(stub: Any, op: str, body: dict[str, Any]) -> dict[str, Any]:
    """Make one ``POST /sockets/<op>`` call through the adapter's grant.

    Args:
        stub: The adapter's ``SocketsTransport`` stub, or ``None`` when not granted.
        op: ``connect``, ``send``, ``close``, ``attachment`` or ``list``.
        body: That operation's JSON fields (bytes already base64).

    Returns:
        The reply body.

    Raises:
        PluginError: ``unavailable`` without a grant, otherwise the host's code.
    """
    if stub is None:
        raise PluginError.from_wire("unavailable", "SOCKETS is not granted to this invocation")
    try:
        raw = await stub.call(op, js(body))
    except PluginError:
        raise
    except Exception as err:  # noqa: BLE001 - an RPC failure has no type of its own
        raise PluginError.from_wire("unavailable", f"sockets {op}: {err}") from err
    reply = py(raw) or {}
    err = _get(reply, "err")
    if err is not None:
        raise PluginError.from_wire(
            str(_get(err, "code", "") or "internal"), str(_get(err, "message", "") or "")
        )
    ok = _get(reply, "ok")
    return dict(ok) if isinstance(ok, dict) else {}


class HostWebSocket:
    """One host-held WebSocket, shaped like a hibernatable Workers ``WebSocket``.

    The broker owns the connection; this handle only names it, so it stays
    valid across invocations and the guest may sleep between messages.

    Attributes:
        id: Socket id the host assigned (``connect`` / ``get_web_sockets``).
        url: URL the socket dialed, without credentials.
        protocol: Subprotocol the server chose, or an empty string.
    """

    def __init__(self, stub: Any, fields: Any) -> None:
        fields = py(fields) or {}
        self.id = str(_get(fields, "socket", "") or "")
        url = _get(fields, "url", "")
        self.url = url if isinstance(url, str) else ""
        protocol = _get(fields, "protocol", "")
        self.protocol = protocol if isinstance(protocol, str) else ""
        self._stub = stub
        attachment = _get(fields, "attachment")
        self._attachment = (
            attachment
            if isinstance(attachment, bytes)
            else _from_b64(attachment)
            if isinstance(attachment, str)
            else _bytes(attachment)
        )

    async def send(self, message: str | bytes) -> None:
        """Send one message.

        Args:
            message: ``str`` (text frame) or bytes (binary frame), at most
                ``MAX_SOCKET_FRAME_BYTES`` bytes.
        """
        await _socket_call(self._stub, "send", {"socket": self.id, "frame": _route_frame(message)})

    async def close(self, code: int = 0, reason: str = "") -> None:
        """Close the socket.

        Args:
            code: Close code; ``0`` sends 1000.
            reason: Close reason, at most 123 UTF-8 bytes.
        """
        await _socket_call(
            self._stub,
            "close",
            {"socket": self.id, "code": max(0, int(code or 0)), "reason": str(reason or "")},
        )

    async def serialize_attachment(self, value: Any) -> None:
        """Store a JSON value with the socket, handed back with every message.

        Args:
            value: JSON-serializable value of at most
                ``MAX_SOCKET_ATTACHMENT_BYTES`` encoded bytes; ``None`` clears it.
        """
        data = encode_socket_attachment(value)
        await _socket_call(self._stub, "attachment", {"socket": self.id, "attachment": _b64(data)})
        self._attachment = data

    def deserialize_attachment(self) -> Any:
        """The stored attachment value, or ``None`` when none is set."""
        return decode_socket_attachment(self._attachment)


class SocketsBinding:
    """``[sockets]`` binding (``env.SOCKETS``): dial host-held WebSockets.

    Every URL must pass the plugin's network grant. Messages arrive on the
    default entrypoint's ``web_socket_message(ws, message)``, closes on
    ``web_socket_close(ws, code, reason, was_clean)``.
    """

    def __init__(self, stub: Any) -> None:
        self._stub = stub

    async def connect(
        self,
        url: str,
        *,
        protocols: list[str] | None = None,
        headers: dict[str, str] | list[tuple[str, str]] | None = None,
        auto_responses: list[tuple[str | bytes, str | bytes]] | None = None,
        attachment: Any = None,
    ) -> HostWebSocket:
        """Open one socket the host holds.

        Args:
            url: ``wss://`` or ``ws://`` URL on the network grant.
            protocols: Requested subprotocols.
            headers: Extra request headers (values may be credentials).
            auto_responses: ``(request, response)`` heartbeat pairs the host
                answers without waking the guest.
            attachment: Initial JSON attachment.

        Returns:
            The socket handle.
        """
        pairs = list(headers.items()) if isinstance(headers, dict) else list(headers or [])
        body = {
            "url": str(url or ""),
            "protocols": [str(p) for p in (protocols or [])],
            "headers": [{"name": str(n), "value": str(v)} for n, v in pairs],
            "autoResponses": [
                {"request": _route_frame(req), "response": _route_frame(resp)}
                for req, resp in (auto_responses or [])
            ],
            "attachment": _b64(encode_socket_attachment(attachment)),
        }
        return HostWebSocket(self._stub, await _socket_call(self._stub, "connect", body))

    async def get_web_sockets(self) -> list[HostWebSocket]:
        """The plugin's open sockets, oldest first."""
        reply = await _socket_call(self._stub, "list", {})
        sockets = reply.get("sockets")
        return (
            [HostWebSocket(self._stub, info) for info in (sockets or [])]
            if isinstance(sockets, list)
            else []
        )


def transport_sockets(stub: Any) -> SocketsBinding:
    """Wrap the adapter's granted sockets stub as a :class:`SocketsBinding`.

    Args:
        stub: ``SocketsTransport``-shaped JS stub from the granted context.

    Returns:
        The author-facing binding.
    """
    return SocketsBinding(stub)


def _sockets_stub(context: Any) -> tuple[str, Any] | None:
    """The granted ``[sockets]`` entry of a context, as ``(name, stub)``."""
    for entry in _get(context, "sockets") or []:
        name = _get(entry, "name")
        stub = _get(entry, "sockets")
        if isinstance(name, str) and name and stub is not None:
            return name, stub
    return None


# ---------------------------------------------------------------------------
# Job trigger: ``job(controller)`` shaped like Workers ``scheduled(controller)``
# ---------------------------------------------------------------------------


class JobController:
    """Everything one job invocation may touch.

    Carries the durable envelope, ``input`` / ``output`` streams, ``progress()``,
    ``cancelled``, and the terminal-outcome recorders ``suspend()`` /
    ``retry_later()``. Returning normally without a recorded outcome completes
    the job; raising rejects it (``unavailable`` / ``deadline_exceeded`` errors
    are retryable, ``cancelled`` is cancelled).
    """

    def __init__(self, invocation: Any, granted: Any = None) -> None:
        inv = py(invocation) or {}
        self.invocation: dict[str, Any] = dict(inv) if isinstance(inv, dict) else {}
        self.id: str = str(_get(inv, "invocationId", ""))
        self.type: str = str(_get(inv, "commandType", ""))
        self.payload_json: str = str(_get(inv, "payloadJson", ""))
        self.payload_schema_version: int = int(_get(inv, "payloadSchemaVersion", 0) or 0)
        self.idempotency_key: str = str(_get(inv, "idempotencyKey", ""))
        self.attempt: int = int(_get(inv, "attempt", 1) or 1)
        self.deadline_unix_ms: int = int(_get(inv, "deadlineUnixMs", 0) or 0)
        self.correlation_id: str = str(_get(inv, "correlationId", ""))
        self.causation_id: str = str(_get(inv, "causationId", ""))
        self.invocation_sequence: int = int(_get(inv, "invocationSequence", 0) or 0)
        self.step_id: str = str(_get(inv, "stepId", ""))
        checkpoint_json = str(_get(inv, "checkpointJson", ""))
        self.checkpoint: dict[str, Any] | None = (
            {
                "json": checkpoint_json,
                "schemaVersion": int(_get(inv, "checkpointSchemaVersion", 0) or 0),
            }
            if checkpoint_json
            else None
        )
        self.input: Any = _get(granted, "input")
        self.output: Any = _get(granted, "output")
        self._progress: Any = _get(granted, "progress")
        self.cancelled: bool = False
        self._result: dict[str, Any] | None = None

    def json(self) -> Any:
        """Decode ``payload_json`` (``{}`` when empty).

        Returns:
            The parsed job payload.
        """
        return json.loads(self.payload_json) if self.payload_json else {}

    async def progress(self, percent: float, message: str = "") -> None:
        """Report ``percent`` in ``0..=100`` with an operator-facing ``message``.

        Args:
            percent: Completion percentage.
            message: Operator-facing status text.
        """
        if self._progress is None:
            return
        await self._progress.report(float(percent or 0), str(message or ""))

    @property
    def result(self) -> dict[str, Any] | None:
        """Recorded terminal outcome (flat ``{"kind": ...}``), or ``None`` while running."""
        return self._result

    def _record(self, result: dict[str, Any]) -> None:
        if self._result is None:
            self._result = result

    def suspend(
        self,
        *,
        checkpoint: Any = None,
        checkpoint_schema_version: int = 1,
        wake_at: datetime | int | float | None = None,
    ) -> None:
        """Release with a bounded checkpoint; the host resumes at ``wake_at``.

        Args:
            checkpoint: JSON-serializable checkpoint (at most ``MAX_CHECKPOINT_BYTES``).
            checkpoint_schema_version: Schema version of ``checkpoint``.
            wake_at: Earliest resume instant (``datetime`` or Unix ms).

        Raises:
            PluginError: ``payload_too_large`` when the checkpoint exceeds the cap.
        """
        self._record(
            {
                "kind": "suspended",
                "checkpoint": {
                    "schemaVersion": int(checkpoint_schema_version or 1),
                    "json": _checkpoint_text(checkpoint),
                },
                "wakeAtUnixMs": _unix_ms(wake_at),
            }
        )

    def retry_later(
        self, *, retry_at: datetime | int | float | None = None, reason: str = ""
    ) -> None:
        """Give up this attempt and let the host retry at ``retry_at`` (or its default).

        Args:
            retry_at: Earliest retry instant (``datetime`` or Unix ms).
            reason: Short operator-facing reason.
        """
        self._record(
            {
                "kind": "retryable",
                "message": str(reason or ""),
                "retryAfterUnixMs": _unix_ms(retry_at),
            }
        )

    def cancel(self) -> None:
        """Host-side cancellation observed (adapter-internal)."""
        self.cancelled = True


def job_outcome_for(
    job: JobController, returned: Any, failed: BaseException | None
) -> dict[str, Any]:
    """Translate a ``job()`` return / raise into the wire ``JobOutcome``.

    Args:
        job: The controller handed to the author.
        returned: The author's return value (``{"message", "bytesCopied"}`` or ``None``).
        failed: Exception raised by the author, if any.

    Returns:
        A flat ``{"kind": ...}`` outcome record.
    """
    if job.result is not None:
        return job.result
    if failed is not None:
        code = getattr(failed, "wire_code", None) or getattr(failed, "code", None)
        if job.cancelled or code == "cancelled":
            return {"kind": "cancelled", "message": _error_message(failed)}
        if code in ("unavailable", "deadline_exceeded"):
            return {"kind": "retryable", "message": _error_message(failed), "retryAfterUnixMs": 0}
        return {"kind": "rejected", "message": _error_message(failed)}
    extra = py(returned) if returned is not None else {}
    if not isinstance(extra, dict):
        extra = {}
    return {
        "kind": "completed",
        "message": str(extra.get("message") or ""),
        "bytesCopied": int(extra.get("bytesCopied") or 0),
    }


@dataclass(frozen=True)
class ScheduledController:
    """One ``[triggers] crons`` fire, as ``scheduled(controller)`` receives it.

    Fires are delivered at least once; a fire that raises is retried with the
    same ``scheduled_time`` and ``fire_id``, so dedupe on ``fire_id``.

    Attributes:
        cron: The schedule that fired, normalized to single spaces.
        scheduled_time: Fire time in UTC Unix milliseconds (a whole minute).
        fire_id: Durable id of this fire, stable across retries.
        attempt: Failure attempt, starting at 1.
        deadline_unix_ms: Deadline hint in Unix milliseconds; the host fence
            is authoritative.
    """

    cron: str = ""
    scheduled_time: int = 0
    fire_id: str = ""
    attempt: int = 0
    deadline_unix_ms: int = 0

    @classmethod
    def from_wire(cls, wire: Any) -> ScheduledController:
        """Builds the controller from a wire ``ScheduledController``.

        Args:
            wire: Decoded wire struct (a JS proxy or a dict); fields may be
                missing.

        Returns:
            The controller with every field present.
        """
        return cls(
            cron=str(_get(wire, "cron", "") or ""),
            scheduled_time=int(_get(wire, "scheduledTime", 0) or 0),
            fire_id=str(_get(wire, "fireId", "") or ""),
            attempt=int(_get(wire, "attempt", 0) or 0),
            deadline_unix_ms=int(_get(wire, "deadlineUnixMs", 0) or 0),
        )


# ---------------------------------------------------------------------------
# Author base classes
# ---------------------------------------------------------------------------


class BookclerkEntrypoint(WorkerEntrypoint):
    """Default-export base for ``api_version = 3`` Python Workers guests.

    Optional trigger methods: ``event(batch)`` (for ``[[events.consumers]]``),
    ``queue(batch)`` (for ``[[queues.consumers]]``), ``scheduled(controller)``
    (for ``[triggers] crons``) and ``job(job)`` (for ``[triggers] jobs``);
    optional ``describe()``
    (presentation fields beyond ``plugin.toml``), ``database_migrations(binding)``,
    and ``shutdown()``. Return plain Python values; the SDK converts them.

    The ``bookclerk*`` methods are the adapter-facing dispatch surface; authors
    neither call nor override them.
    """

    async def fetch(self, _request=None):
        """Reject HTTP fetch — workerd guests are Workers-RPC only.

        Args:
            _request: Incoming HTTP request (unused by the default stub).

        Returns:
            A 404 ``Response``.
        """
        return Response.new(None, {"status": 404})

    async def bookclerkDescribe(self):
        """Adapter dispatch: the author's optional ``describe()`` (or ``None``)."""
        describe = getattr(self, "describe", None)
        if not callable(describe):
            return None
        return js(await _await_maybe(describe()))

    async def bookclerkEvent(self, context=None, wire_batch=None):
        """Adapter dispatch: deliver one ``EventBatch`` to ``event(batch)``.

        Raises:
            PluginError: ``unsupported`` when the author defines no ``event``.
        """
        handler = getattr(self, "event", None)
        if not callable(handler):
            raise _unsupported("event")
        _apply_invocation_env(self, context)
        batch = EventBatch(_get(wire_batch, "events", []) or [], _invocation_of(context))
        try:
            await _await_maybe(handler(batch))
        except Exception as err:  # noqa: BLE001 - every failure becomes a retry
            return js(event_batch_results(batch, err))
        return js(event_batch_results(batch, None))

    async def bookclerkQueue(self, context=None, wire_batch=None):
        """Adapter dispatch: deliver one ``QueueBatch`` to ``queue(batch)``.

        Raises:
            PluginError: ``unsupported`` when the author defines no ``queue``.
        """
        handler = getattr(self, "queue", None)
        if not callable(handler):
            raise _unsupported("queue")
        _apply_invocation_env(self, context)
        batch = QueueBatch(
            _get(wire_batch, "queue", ""),
            _get(wire_batch, "messages", []) or [],
            _invocation_of(context),
        )
        try:
            await _await_maybe(handler(batch))
        except Exception as err:  # noqa: BLE001 - every failure becomes a retry
            return js(queue_batch_results(batch, err))
        return js(queue_batch_results(batch, None))

    async def bookclerkScheduled(self, context: Any = None, wire_controller: Any = None) -> None:
        """Adapter dispatch: run one cron fire through ``scheduled(controller)``.

        The handler receives a :class:`ScheduledController`. Returning
        completes the fire; raising has the host retry the same fire.

        Raises:
            PluginError: ``unsupported`` when the author defines no ``scheduled``.
        """
        handler = getattr(self, "scheduled", None)
        if not callable(handler):
            raise _unsupported("scheduled")
        _apply_invocation_env(self, context)
        await _await_maybe(handler(ScheduledController.from_wire(wire_controller)))

    def _socket_handle(self, context: Any, wire: Any) -> HostWebSocket:
        """The :class:`HostWebSocket` a socket event names."""
        entry = _sockets_stub(context)
        return HostWebSocket(
            entry[1] if entry else None,
            {"socket": _get(wire, "socket", ""), "attachment": _get(wire, "attachment")},
        )

    async def bookclerkSocketMessage(self, context: Any = None, wire: Any = None) -> Any:
        """Adapter dispatch: one host-held socket message to ``web_socket_message(ws, message)``.

        Text arrives as ``str``, binary as ``bytes``. The handler may return
        ``{"poll_events": True}`` (or ``pollEvents``) when the message announced
        something ``poll_events`` hands over, such as a new user; the host then
        polls now instead of at its next scheduled poll.

        Returns:
            The wire ``SocketMessageOk``: ``{"pollEvents": bool}``.

        Raises:
            PluginError: ``unsupported`` when the author defines no handler.
        """
        handler = getattr(self, "web_socket_message", None) or getattr(
            self, "webSocketMessage", None
        )
        if not callable(handler):
            raise _unsupported("webSocketMessage")
        _apply_invocation_env(self, context)
        wire = py(wire) or {}
        asked = py(
            await _await_maybe(
                handler(
                    self._socket_handle(context, wire), socket_message_data(_get(wire, "frame"))
                )
            )
        )
        return js({"pollEvents": socket_message_asks_poll(asked)})

    async def bookclerkSocketClosed(self, context: Any = None, wire: Any = None) -> None:
        """Adapter dispatch: a host-held socket closed, to ``web_socket_close(ws, code, reason, was_clean)``.

        Without a handler the close is acknowledged and ignored.
        """
        handler = getattr(self, "web_socket_close", None) or getattr(self, "webSocketClose", None)
        if not callable(handler):
            return
        _apply_invocation_env(self, context)
        wire = py(wire) or {}
        await _await_maybe(
            handler(
                self._socket_handle(context, wire),
                int(_get(wire, "code", 0) or 0),
                str(_get(wire, "reason", "") or ""),
                _get(wire, "clean") is True,
            )
        )

    async def bookclerkJob(self, context=None, invocation=None, granted=None):
        """Adapter dispatch: run one durable job through ``job(controller)``.

        Raises:
            PluginError: ``unsupported`` when the author defines no ``job``.
        """
        handler = getattr(self, "job", None)
        if not callable(handler):
            raise _unsupported("job")
        _apply_invocation_env(self, context)
        job = JobController(invocation, granted)
        try:
            returned = await _await_maybe(handler(job))
        except Exception as err:  # noqa: BLE001 - every failure becomes an outcome
            return js(job_outcome_for(job, None, err))
        return js(job_outcome_for(job, returned, None))

    async def bookclerkDatabaseMigrations(self, binding=""):
        """Adapter dispatch: the author's ``database_migrations(binding)`` (or ``[]``)."""
        from .plugin_migration import require_plugin_migration_registration

        handler = getattr(self, "database_migrations", None) or getattr(
            self, "databaseMigrations", None
        )
        if not callable(handler):
            return js([])
        migrations = py(await _await_maybe(handler(str(binding or ""))))
        return js(require_plugin_migration_registration(migrations or []))

    async def bookclerkShutdown(self):
        """Adapter dispatch: the author's optional ``shutdown()``."""
        handler = getattr(self, "shutdown", None)
        if callable(handler):
            await _await_maybe(handler())
        return None


class NamedEntrypoint(WorkerEntrypoint):
    """Base for named entrypoints.

    Subclasses list their RPC surface on ``bookclerk_methods``; the adapter
    reaches them only through :meth:`bookclerkInvoke`, which installs the
    granted bindings on ``env`` before dispatching. Method arguments arrive as
    plain Python values (``py``) and return values are converted with ``js``.
    """

    bookclerk_methods: tuple[str, ...] = ()

    async def fetch(self, _request=None):
        """Reject HTTP fetch — workerd guests are Workers-RPC only.

        Args:
            _request: Incoming HTTP request (unused by the default stub).

        Returns:
            A 404 ``Response``.
        """
        return Response.new(None, {"status": 404})

    async def bookclerkInvoke(self, context=None, method="", *args):
        """Adapter dispatch: call ``method`` with the granted bindings installed.

        Raises:
            PluginError: ``unsupported`` for methods outside ``bookclerk_methods``.
        """
        method = str(method)
        target = getattr(self, method, None)
        if method not in type(self).bookclerk_methods or not callable(target):
            raise _unsupported(method)
        _apply_invocation_env(self, context)
        self.invocation = _invocation_of(context)
        result = await _await_maybe(target(*(py(a) for a in args)))
        return js(result)


PURCHASE_HINTS_BUDGET_SECONDS = 4.0
"""How long the default ``purchaseHints`` keeps starting one-title lookups
before answering with what it has. Well under the host's 10 s batch
deadline, so even a slow last lookup lands before it and a store without a
bulk endpoint still returns the titles it priced."""

PURCHASE_HINTS_CONCURRENCY = 4
"""How many one-title lookups the default ``purchaseHints`` keeps in flight
at once. A store without a bulk endpoint answers a page of cards in about a
quarter of the time one lookup after another takes, while staying a polite
handful of requests rather than a burst."""

STOREFRONT_METHODS: tuple[str, ...] = (
    "login",
    "scan",
    "fetchTitle",
    "listAccounts",
    "loginStart",
    "loginComplete",
    "searchCatalog",
    "expandCandidates",
    "purchaseHint",
    "purchaseHints",
    "listDeals",
    "catalogDetail",
    "revokeAccount",
    "qualityOffers",
    "diagnose",
    "health",
)
"""RPC surface of the ``storefront`` entrypoint."""


class StorefrontEntrypoint(NamedEntrypoint):
    """``storefront`` entrypoint (``ContentSource`` interface).

    Every method takes and returns the generated :mod:`bookclerk_plugin_sdk.abi`
    TypedDict shapes as plain dicts (``LoginParams`` → ``LoginResult``, …).
    Override only the operations the storefront supports; the rest raise
    ``unsupported``.
    """

    bookclerk_methods = STOREFRONT_METHODS

    async def login(self, _params: abi.LoginParams):
        """Password or one-shot OAuth login (``LoginParams`` → ``LoginResult``)."""
        raise _unsupported("login")

    async def scan(self, _params: abi.ScanParams):
        """Return one page of one account's library (``ScanParams`` → ``ScanPage``)."""
        raise _unsupported("scan")

    async def fetchTitle(self, _params: abi.FetchTitleParams):
        """Fetch one title's media and metadata (``FetchTitleParams`` → ``PlainFetch``)."""
        raise _unsupported("fetchTitle")

    async def listAccounts(self):
        """List accounts known to this storefront (list of ``SourceAccount``)."""
        raise _unsupported("listAccounts")

    async def loginStart(self, _params: abi.LoginParams):
        """Begin an interactive OAuth login (``LoginParams`` → ``LoginStartResult``)."""
        raise _unsupported("loginStart")

    async def loginComplete(self, _params: abi.LoginCompleteParams):
        """Finish an interactive OAuth login (``LoginCompleteParams`` → ``LoginResult``)."""
        raise _unsupported("loginComplete")

    async def searchCatalog(self, _params: abi.SearchCatalogParams):
        """Free-text catalog search (``SearchCatalogParams`` → list of ``CatalogHit``)."""
        raise _unsupported("searchCatalog")

    async def expandCandidates(self, _params: abi.ExpandCandidatesParams):
        """Related-title expansion (``ExpandCandidatesParams`` → list of ``CatalogHit``)."""
        raise _unsupported("expandCandidates")

    async def purchaseHint(self, _params: abi.PurchaseHintParams):
        """Purchase link / price hint (``PurchaseHintParams`` → ``PurchaseHint``).

        ``params["lookup"]`` says how the host names the title (``"product"``,
        ``"isbn"``, ``"asin"``, ``"title"`` or ``"unspecified"``). For a
        ``"title"`` lookup, return ``{"hint": ..., "records": [...]}`` with
        every priced listing the search matched (each ``{"record":
        CatalogHit, "hint": PurchaseHint}``) so the host can keep the
        recording it asked for; a bare ``PurchaseHint`` or ``None`` also works.
        """
        raise _unsupported("purchaseHint")

    async def purchaseHints(self, params: abi.PurchaseHintsParams):
        """Purchase links / price hints for several titles at once.

        Asks :meth:`purchaseHint` for each query,
        :data:`PURCHASE_HINTS_CONCURRENCY` at a time, those that already name
        a product id first (one lookup each) and title searches after, and
        starts nothing once :data:`PURCHASE_HINTS_BUDGET_SECONDS` has passed,
        so the host's batch gets what the store managed instead of nothing.
        A store whose catalog prices many products per request overrides
        this to make one request. Raise a ``rate_limited``
        :class:`PluginError` (with ``details.retryAfterMs``) when the store
        asks to slow down.

        Args:
            params: The titles to price, in order.

        Returns:
            One ``PurchaseHint`` or ``None`` per query.
        """
        started = time.monotonic()
        queries = params["queries"]
        out: list[abi.PurchaseHint | None] = [None] * len(queries)
        order = sorted(
            range(len(queries)),
            key=lambda i: not (queries[i].get("productId") or "").strip(),
        )
        pending = iter(order)

        async def lane() -> None:
            # Each lane takes the next query in order until the budget runs out.
            for i in pending:
                if time.monotonic() - started >= PURCHASE_HINTS_BUDGET_SECONDS:
                    return
                out[i] = await self.purchaseHint(queries[i])

        lanes = [
            asyncio.ensure_future(lane())
            for _ in range(min(PURCHASE_HINTS_CONCURRENCY, len(order)))
        ]
        try:
            # The first failure (a rate limit included) fails the batch.
            await asyncio.gather(*lanes)
        finally:
            for task in lanes:
                task.cancel()
        return out

    async def listDeals(self, _params: abi.ListDealsParams):
        """Current storefront deals (``ListDealsParams`` → list of ``CatalogHit``)."""
        raise _unsupported("listDeals")

    async def catalogDetail(self, _params: abi.CatalogDetailParams):
        """Full catalog record (``CatalogDetailParams`` → ``CatalogHit``)."""
        raise _unsupported("catalogDetail")

    async def revokeAccount(self, _params: abi.RevokeAccountParams) -> None:
        """Sign the account out at the store before the host deletes its credentials.

        Best effort: the host revokes the account whatever this returns. The
        default does nothing (``RevokeAccountParams`` → ``None``).
        """
        return None

    async def qualityOffers(self, _params: abi.QualityOffersParams) -> abi.QualityOffersResult:
        """Audio quality one account would download for each title.

        One answer per title id in order (``found`` false when the store
        cannot say); it must not license or download anything. The host asks
        only a store that declares ``catalog.qualityOffers``; the default
        raises unsupported (``QualityOffersParams`` → ``QualityOffersResult``).
        """
        raise _unsupported("qualityOffers")

    async def health(self):
        """Liveness / readiness probe (``HealthOk``)."""
        return {"ok": True, "detail": ""}

    async def diagnose(self):
        """Operator-facing diagnostic lines (list of strings)."""
        return []


STORAGE_METHODS: tuple[str, ...] = (
    "head",
    "list",
    "get",
    "put",
    "copy",
    "delete",
    "commit",
    "abortStage",
    "move",
    "moveMany",
    "deleteMany",
    "headMany",
    "changes",
    "translateKeys",
)
"""RPC surface of the ``storage`` entrypoint."""


class StorageEntrypoint(NamedEntrypoint):
    """``storage`` entrypoint (``Destination`` interface): an object store the host writes to."""

    bookclerk_methods = STORAGE_METHODS

    async def head(self, _key: str):
        """Object metadata or ``None``."""
        raise _unsupported("head")

    async def list(self, _options: Any):
        """One page of objects under a prefix."""
        raise _unsupported("list")

    async def get(self, _key: str, _options: Any = None):
        """Read an object (optionally a byte range)."""
        raise _unsupported("get")

    async def put(self, _key: str, _body: Any, _options: Any = None):
        """Write an object from a byte stream."""
        raise _unsupported("put")

    async def copy(self, _from_key: str, _to_key: str):
        """Server-side copy."""
        raise _unsupported("copy")

    async def delete(self, _key: str):
        """Delete an object."""
        raise _unsupported("delete")

    async def commit(self, _key: str, _commit_token: str):
        """Publish a staged object."""
        raise _unsupported("commit")

    async def abortStage(self, _key: str, _commit_token: str):
        """Discard a staged object."""
        raise _unsupported("abortStage")

    async def move(self, _from_key: str, _to_key: str, _options: Any = None):
        """Move one object (declare ``storage.move``).

        ``options`` may carry ``noOverwrite`` and ``ifSourceMatch``. Returns
        ``{"atomic": bool, "alreadyMoved": bool}``; ``alreadyMoved`` reports a
        retried move whose source is gone and whose target exists.
        """
        raise _unsupported("move")

    async def moveMany(self, _request: Any):
        """Move explicit pairs or one page of a prefix (declare ``storage.moveMany``).

        ``request["target"]`` is ``{"kind": "pairs", "value": [...]}`` (at
        most ``MAX_BATCH_KEYS``) or ``{"kind": "prefix", "value": {...}}``,
        paged through ``cursor``. Returns ``{"results", "nextCursor",
        "atomic", "objectsMoved"}``, one result per pair in order.
        """
        raise _unsupported("moveMany")

    async def deleteMany(self, _keys: list[str]):
        """Delete up to ``MAX_BATCH_KEYS`` objects (declare ``storage.deleteMany``).

        Returns one ``{"key", "error"?}`` outcome per key, in order; a
        missing object counts as deleted.
        """
        raise _unsupported("deleteMany")

    async def headMany(self, _keys: list[str]):
        """Metadata for up to ``MAX_BATCH_KEYS`` objects (declare ``storage.headMany``).

        Returns one entry per key, in order: object metadata or ``None``.
        """
        raise _unsupported("headMany")

    async def changes(self, _request: Any):
        """One page of the change feed after a cursor (declare ``storage.changes``).

        Returns ``{"changes", "cursor", "more", "complete"}``; ``complete``
        is false when changes may have been missed and the host must relist.
        """
        raise _unsupported("changes")

    async def translateKeys(self, _keys: list[str]):
        """Translate up to ``MAX_BATCH_KEYS`` host keys (declare ``storage.translateKeys``).

        A host key is ``/``-joined segments, none empty, ``.``, ``..`` or
        holding a backslash or a control character. Returns one relative key
        per host key, in order: the key this destination stores it under.
        The same host key must always translate to the same key; without the
        feature the host stores each host key as written.
        """
        raise _unsupported("translateKeys")


REMOTE_LIBRARY_METHODS: tuple[str, ...] = (
    "health",
    "diagnose",
    "start",
    "stop",
    "scanLibrary",
    "syncListening",
    "pollEvents",
)
"""RPC surface of the ``remoteLibrary`` entrypoint."""


class RemoteLibraryEntrypoint(NamedEntrypoint):
    """``remoteLibrary`` entrypoint: lifecycle, rescan, listening sync, user polling."""

    bookclerk_methods = REMOTE_LIBRARY_METHODS

    async def health(self):
        """Liveness / readiness probe (``HealthOk``)."""
        return {"ok": True, "detail": ""}

    async def diagnose(self):
        """Operator-facing diagnostic lines (list of strings)."""
        return []

    async def start(self):
        """Start background work after the host has granted bindings."""
        return None

    async def stop(self):
        """Stop background work."""
        return None

    async def scanLibrary(self, _params: abi.ScanLibraryParams):
        """Re-sync the remote library."""
        raise _unsupported("scanLibrary")

    async def syncListening(self):
        """Push / pull listening progress (list of ``ListeningProgress``)."""
        raise _unsupported("syncListening")

    async def pollEvents(self):
        """``ExternalUser`` records observed since the last poll."""
        raise _unsupported("pollEvents")


_ADAPTER_SESSIONS: dict[str, Any] = {}
"""Open adapter sessions by id.

Workers RPC stubs cannot be retained across bridge requests, so the session
object stays in the author isolate and the launcher names it through
``X-Bookclerk-Target``.
"""

_ADAPTER_SESSION_METHODS: frozenset[str] = frozenset(
    {
        "capabilities",
        "execute",
        "close",
        "bootstrap",
        "exportIdentity",
        "importIdentity",
        "listUserRelations",
        "prepareUnitRestore",
        "dropUserRelations",
        "assertRestoreConstraints",
        "executeStreamed",
    }
)


def _fresh_session_id() -> str:
    import secrets

    return secrets.token_hex(16)


class DatabaseAdapterEntrypoint(NamedEntrypoint):
    """``databaseAdapter`` entrypoint: opens typed SQL sessions for the host library.

    The adapter isolate drives sessions through two dispatch verbs on
    :meth:`bookclerkInvoke`: ``openSession`` returns a fresh session id, and
    ``session(id, method, *params)`` calls ``method`` on the retained
    :class:`AdapterDatabaseSession`; ``close`` releases the entry.
    """

    bookclerk_methods = ("openSession", "dropUnit")

    async def openSession(self):
        """Open an :class:`AdapterDatabaseSession`."""
        raise _unsupported("openSession")

    async def dropUnit(self, _unit_ref: str):
        """Physically drop one provisioned binding unit.

        Args:
            _unit_ref: Adapter-native unit identity.

        Returns:
            ``None`` when the unit is dropped.

        Raises:
            PluginError: ``unsupported`` unless a subclass implements the drop.
        """
        raise _unsupported("dropUnit")

    async def bookclerkInvoke(self, context=None, method="", *args):
        """Adapter dispatch: ``openSession`` → session id; ``dropUnit``; ``session`` → call on a retained session.

        Args:
            context: Granted bindings for this invocation.
            method: ``openSession``, ``dropUnit``, or ``session``.
            *args: For ``session``: ``(id, session_method, *params)``.

        Returns:
            The session id (``str``) for ``openSession``, else the session
            method result converted with :func:`js`.

        Raises:
            PluginError: ``invalid_params`` for an unknown session id,
                ``unsupported`` for a method outside the session surface.
        """
        method = str(method)
        if method == "openSession":
            session = await super().bookclerkInvoke(context, "openSession")
            if session is None:
                raise PluginError.from_wire("internal", "openSession returned no session")
            session_id = _fresh_session_id()
            _ADAPTER_SESSIONS[session_id] = session
            return session_id
        if method == "dropUnit":
            _apply_invocation_env(self, context)
            self.invocation = _invocation_of(context)
            unit_ref = str(py(args[0])) if args else ""
            await _await_maybe(self.dropUnit(unit_ref))
            return None
        if method == "session":
            if not args:
                raise PluginError.from_wire("invalid_params", "session requires an id")
            session_id = py(args[0])
            session = _ADAPTER_SESSIONS.get(session_id) if isinstance(session_id, str) else None
            if session is None:
                raise PluginError.from_wire(
                    "invalid_params", f"unknown adapter session {session_id!r}"
                )
            name = str(py(args[1])) if len(args) > 1 else ""
            target = getattr(session, name, None)
            if name not in _ADAPTER_SESSION_METHODS or not callable(target):
                raise _unsupported(name)
            _apply_invocation_env(self, context)
            self.invocation = _invocation_of(context)
            try:
                result = await _await_maybe(target(*(py(a) for a in args[2:])))
            finally:
                if name == "close":
                    _ADAPTER_SESSIONS.pop(session_id, None)
            return js(result)
        return await super().bookclerkInvoke(context, method, *args)


class CliEntrypoint(NamedEntrypoint):
    """``cli`` entrypoint (``PluginCli``): ``describe()`` → ``CliSchema``, ``invoke(params)`` → ``CliInvokeResult``."""

    bookclerk_methods = ("describe", "invoke")

    async def describe(self):
        """Guest CLI schema (``{"commands": []}`` when the guest has no CLI)."""
        return {"commands": []}

    async def invoke(self, _params: abi.CliInvokeParams):
        """Invoke a guest CLI command (use :func:`cli_args` / :func:`json_payload`)."""
        raise _unsupported("invoke")


class OidcEntrypoint(NamedEntrypoint):
    """``oidc`` entrypoint: relying-party client templates and credential verification."""

    bookclerk_methods = ("clients", "authenticateUser")

    async def clients(self):
        """Plugin-provided OIDC client templates (list of ``OidcClientTemplate``)."""
        return []

    async def authenticateUser(self, _params: abi.AuthenticateUserParams):
        """Verify remote credentials on behalf of the host (``ExternalUser``)."""
        raise _unsupported("authenticateUser")


class MetadataEntrypoint(NamedEntrypoint):
    """``metadata`` entrypoint: bibliographic lookups against a public catalog.

    Records describe editions; the host decides identity.
    """

    bookclerk_methods = ("health", "lookup", "search", "chapters")

    async def health(self) -> abi.HealthOk:
        """Liveness / readiness probe (``HealthOk``)."""
        return {"ok": True, "detail": ""}

    async def lookup(self, _params: abi.MetadataLookupParams) -> list[abi.MetadataRecord]:
        """Records for the editions the identifiers name (list of ``MetadataRecord``)."""
        raise _unsupported("lookup")

    async def search(self, _params: abi.MetadataSearchParams) -> list[abi.MetadataRecord]:
        """Records matching a title and author (list of ``MetadataRecord``)."""
        raise _unsupported("search")

    async def chapters(self, _params: abi.MetadataChaptersParams) -> abi.MetadataChapters:
        """Chapter layout of one edition (``MetadataChapters``)."""
        raise _unsupported("chapters")


class CoverEntrypoint(NamedEntrypoint):
    """``covers`` entrypoint: downloads cover images for the host.

    The host passes a ``sink`` destination; put the body for ``urls[i]`` under
    the key ``str(i)`` and return one ``CoverImage`` per URL, in request
    order. The host sniffs, decodes and re-encodes every body.
    """

    bookclerk_methods = ("health", "fetchCovers")

    async def health(self) -> abi.HealthOk:
        """Liveness / readiness probe (``HealthOk``)."""
        return {"ok": True, "detail": ""}

    async def fetchCovers(self, _params: abi.CoverFetchParams, _sink: Any) -> list[abi.CoverImage]:
        """Fetch each URL into ``_sink`` under its index (list of ``CoverImage``)."""
        raise _unsupported("fetchCovers")


class MetadataFilesEntrypoint(NamedEntrypoint):
    """``metadataFiles`` entrypoint: reads and writes metadata files for the host.

    The host reads every file, chooses every path and writes every result, so
    the entrypoint only converts bytes. Each dialect is a
    ``[[metadata_files.dialects]]`` row in ``plugin.toml``; ``params["dialect"]``
    names the row.
    """

    bookclerk_methods = ("health", "parse", "render")

    async def health(self) -> abi.HealthOk:
        """Liveness / readiness probe (``HealthOk``)."""
        return {"ok": True, "detail": ""}

    async def parse(self, _params: abi.MetadataFileParseParams) -> abi.MetadataFileRecord:
        """What one file says (``MetadataFileRecord``).

        List every field the file states in ``present``, including ones it
        states as empty. Raise ``invalid_params`` for a file the dialect cannot
        read.
        """
        raise _unsupported("parse")

    async def render(self, _params: abi.MetadataFileRenderParams) -> bytes | str:
        """The new file: ``existing`` with ``fields`` set from ``record``.

        Everything else in the file is kept; a ``str`` is written as UTF-8.
        """
        raise _unsupported("render")


class NotifierEntrypoint(NamedEntrypoint):
    """``notifier`` entrypoint: delivers messages the host already rendered.

    The host decides what to send and where; the entrypoint only sends to
    one outside service (a chat webhook, a mail relay, a paging service).
    ``[notifier]`` in ``plugin.toml`` declares the destination kinds, formats
    and personal address fields.
    """

    bookclerk_methods = ("health", "send", "validateDestination")

    async def health(self) -> abi.HealthOk:
        """Liveness / readiness probe (``HealthOk``)."""
        return {"ok": True, "detail": ""}

    async def send(self, _params: abi.NotifySendParams) -> abi.NotifySent | None:
        """Deliver one message to one destination (``NotifySent``).

        Raise ``rate_limited`` (with ``retryAfterMs`` in the details when the
        service says when) or ``unavailable`` for a failure worth retrying; any
        other error fails this destination. Never log the message when
        ``sensitive`` is set, nor ``addressJson``. ``None`` means the service
        gave no message id.
        """
        raise _unsupported("send")

    async def validateDestination(self, params: abi.NotifyValidateParams) -> abi.NotifyValidated:
        """Check and normalise a personal address (``NotifyValidated``).

        Raise ``invalid_params`` for an address the service would refuse. The
        default accepts the address as given.
        """
        return {"addressJson": params["addressJson"]}


class IngressEntrypoint(NamedEntrypoint):
    """``ingress`` entrypoint: gives the library a public ``https`` address.

    The host starts at most one ingress instance at a time. The Python SDK
    serves ``[ingress] kind = "direct"`` plugins (dynamic DNS, port mapping),
    which keep the address current and never receive requests; the ``ORIGIN``
    binding a relay plugin hands requests to is provided by the TypeScript
    SDK only.
    """

    bookclerk_methods = ("start", "health", "stop")

    async def start(self) -> abi.IngressStarted:
        """Bring the address up and return it once it serves (``IngressStarted``).

        The origin is ``https://`` and a host name, with no port, path or
        trailing slash.
        """
        raise _unsupported("start")

    async def health(self) -> abi.IngressHealth:
        """Whether the address is serving (``IngressHealth``)."""
        return {"serving": True}

    async def stop(self) -> None:
        """Take the address down; the host stops relying on it after this returns."""
        return None


# ---------------------------------------------------------------------------
# Database capabilities
# ---------------------------------------------------------------------------


class GuestDatabase:
    """Host-granted SQL transport behind a named ``[[databases]]`` binding."""

    async def execute(self, _request: ExecuteRequest) -> ExecuteReply:
        """Host-mediated typed batch (``ExecuteRequest`` → ``ExecuteReply``)."""
        raise _unsupported("execute")

    async def execute_streamed(self, _request: ExecuteRequest) -> bytes | AsyncIterable[bytes]:
        """The same batch answered as ``ExecuteStreamFrame`` bytes.

        The reply is counted against the session's ``maxStreamedResultBytes``
        / ``maxStreamedResultRows`` instead of the inline caps, so a result
        past them still arrives whole.

        Returns:
            Every frame, concatenated, or the frames as a byte stream.

        Raises:
            PluginError: ``unsupported`` on a host that predates the method.
        """
        raise _unsupported("executeStreamed")

    async def close(self) -> None:
        """Close the grant."""
        return None

    async def limits(self) -> DbCapabilities:
        """Negotiated limits of this session (``DbCapabilities``).

        Returns:
            The adapter's capability advertisement for this session.

        Raises:
            PluginError: ``unsupported`` on a host that predates the method.
        """
        raise _unsupported("limits")


class AdapterDatabaseSession:
    """Host ↔ database adapter session (``capabilities`` + typed ``execute``).

    The restore / identity methods are optional; the base class raises
    ``unsupported`` for each.
    """

    async def capabilities(self) -> Any:
        """Typed SQL-contract advertisement."""
        raise _unsupported("capabilities")

    async def execute(self, _request: ExecuteRequest) -> ExecuteReply:
        """Typed atomic batch (``ExecuteRequest`` → ``ExecuteReply``)."""
        raise _unsupported("execute")

    async def executeStreamed(self, _request: abi.AdapterExecuteRequest) -> bytes:
        """The same atomic batch as ``execute``, answered as ``ExecuteStreamFrame`` bytes.

        The frames are counted against the advertised ``maxStreamedResultBytes``
        / ``maxStreamedResultRows``; adapters that advertise ``0`` for both
        keep this default, which answers ``unsupported``. The request carries
        the proven statements plus ``isolation`` and ``receipt``, like the
        adapter-side ``execute`` envelope. An isolate adapter's frames travel
        inside one invoke reply, so they must fit ``MAX_SCALAR_BYTES``;
        ``db_value.execute_adapter_batch_streamed`` runs the batch, stores the
        replay receipt and checks that bound and the advertised streamed
        budget before it commits.

        Returns:
            Every frame of the committed reply, concatenated.
        """
        raise _unsupported("executeStreamed")

    async def close(self) -> None:
        """Close the session."""
        return None

    async def bootstrap(self) -> Any:
        """Bootstrap-only diagnostic metadata (``DbBootstrap``)."""
        raise _unsupported("bootstrap")

    async def exportIdentity(self) -> list[Any]:
        """Identity high-water marks for a logical export (``IdentityHighWater`` rows)."""
        raise _unsupported("exportIdentity")

    async def importIdentity(self, _rows: list[Any]) -> None:
        """Restore identity high-water marks after a logical import."""
        raise _unsupported("importIdentity")

    async def listUserRelations(self) -> list[str]:
        """Names of user relations the host may drop before a restore."""
        raise _unsupported("listUserRelations")

    async def prepareUnitRestore(self) -> None:
        """Prepare the engine for a unit restore."""
        raise _unsupported("prepareUnitRestore")

    async def dropUserRelations(self, _names: list[str]) -> None:
        """Drop the named user relations."""
        raise _unsupported("dropUserRelations")

    async def assertRestoreConstraints(self) -> None:
        """Re-check constraints after a unit restore."""
        raise _unsupported("assertRestoreConstraints")


class _GrantedFetcher(Protocol):
    async def fetch(self, url: str, /, **kwargs: Any) -> Any: ...


class _GrantedGuestDatabase(GuestDatabase):
    """Granted-channel ``GuestDatabase`` over ``POST /db/execute``."""

    def __init__(
        self,
        granted: _GrantedFetcher,
        grant_token: str,
        signal: Any | None = None,
    ) -> None:
        self._granted = granted
        self._auth = {"Authorization": f"Bearer {grant_token}"}
        self._signal = signal

    async def execute(self, request: ExecuteRequest) -> ExecuteReply:
        from bookclerk_plugin_sdk.db_value import (
            decode_execute_result_reply,
            encode_execute_request,
        )

        body = encode_execute_request(request)
        kwargs: dict[str, Any] = {
            "method": "POST",
            "headers": {**self._auth, "content-type": "application/octet-stream"},
            "body": body,
        }
        if self._signal is not None:
            kwargs["signal"] = self._signal
        resp = await self._granted.fetch("http://granted/db/execute", **kwargs)
        data = await _granted_database_body(resp, "execute")
        try:
            return decode_execute_result_reply(data)
        except PluginError:
            raise
        except Exception as err:
            raise PluginError.from_wire("internal", str(err)) from err

    async def execute_streamed(self, request: ExecuteRequest) -> bytes | AsyncIterable[bytes]:
        """The same batch over ``POST /db/execute-streamed``.

        Args:
            request: Statements, binds and deadline.

        Returns:
            The response body, chunk by chunk when the channel streams it.
        """
        from bookclerk_plugin_sdk.db_value import encode_execute_request

        body = encode_execute_request(request)
        kwargs: dict[str, Any] = {
            "method": "POST",
            "headers": {**self._auth, "content-type": "application/octet-stream"},
            "body": body,
        }
        if self._signal is not None:
            kwargs["signal"] = self._signal
        resp = await self._granted.fetch("http://granted/db/execute-streamed", **kwargs)
        status = getattr(resp, "status", None)
        if status is not None and int(status) >= 400:
            await _granted_database_body(resp, "executeStreamed")
        return _granted_body_chunks(resp, "executeStreamed")

    async def limits(self) -> DbCapabilities:
        """Negotiated limits of the granted session (``GET /db/limits``).

        Returns:
            The adapter's ``DbCapabilities`` for this binding.
        """
        from bookclerk_plugin_sdk.db_value import decode_db_capabilities_reply

        kwargs: dict[str, Any] = {"method": "GET", "headers": {**self._auth}}
        if self._signal is not None:
            kwargs["signal"] = self._signal
        resp = await self._granted.fetch("http://granted/db/limits", **kwargs)
        data = await _granted_database_body(resp, "limits")
        try:
            return decode_db_capabilities_reply(data)
        except PluginError:
            raise
        except Exception as err:
            raise PluginError.from_wire("internal", str(err)) from err


async def _granted_database_body(resp: Any, method: str) -> bytes:
    """Read a granted ``/db/*`` response body.

    A non-2xx status maps onto the ``PluginError`` the channel implies:
    ``401`` is ``unauthorized``, ``413`` is ``limit_exceeded`` (the request was
    over the grant's ``maxRequestBytes``; the body is the canonical limit
    message, parsed back into ``details``), ``404`` and ``405`` are
    ``unsupported`` (an older host without the route) and anything else
    ``unavailable``.

    Args:
        resp: Granted-channel response.
        method: ``GuestDatabase`` method name for the error text.

    Returns:
        Response body bytes.

    Raises:
        PluginError: When the status is an error or the body is missing.
    """
    status = getattr(resp, "status", None)
    if status is not None and int(status) >= 400:
        text = await resp.text() if hasattr(resp, "text") else str(resp)
        message = f"database grant: {status} {text}".strip()
        if int(status) == 413:
            limit = parse_limit_exceeded(text)
            if limit is None:
                raise PluginError.from_wire("limit_exceeded", message)
            raise PluginError("limit_exceeded", message, limit_exceeded_details(limit))
        if int(status) in (404, 405):
            raise PluginError.from_wire(
                "unsupported",
                f"GuestDatabase.{method} is not served by this host ({message})",
            )
        code = "unauthorized" if int(status) == 401 else "unavailable"
        raise PluginError.from_wire(code, message)
    if hasattr(resp, "arrayBuffer"):
        raw = await resp.arrayBuffer()
        return bytes(raw)
    if hasattr(resp, "body"):
        return bytes(await resp.body)
    raise PluginError.from_wire("internal", f"granted {method} response missing body")


async def _reader_chunks(stream: Any) -> AsyncIterator[bytes]:
    """Read a web ``ReadableStream`` window by window.

    Args:
        stream: Object with a ``getReader()`` whose reader answers ``read()``
            with ``done`` / ``value`` records.

    Yields:
        Each non-empty window as ``bytes``, in order.
    """
    reader = stream.getReader()
    drained = False
    try:
        while True:
            result = await reader.read()
            if bool(getattr(result, "done", False)):
                drained = True
                return
            value = getattr(result, "value", None)
            if value is not None:
                yield _bytes(value)
    finally:
        # A consumer that stops early (an `iterate()` loop that breaks, a
        # decoder that refused a frame) cancels the unread body and releases
        # the reader instead of leaving the transfer parked behind the lock.
        if not drained:
            await _cancel_reader(reader)
        release = getattr(reader, "releaseLock", None)
        if release is not None:
            release()


async def _cancel_reader(reader: Any) -> None:
    """Cancel a web ``ReadableStreamDefaultReader``, ignoring a transport that refuses.

    Args:
        reader: The reader to cancel; one without ``cancel()`` is left as is.
    """
    cancel = getattr(reader, "cancel", None)
    if cancel is None:
        return
    try:
        result = cancel()
        if inspect.isawaitable(result):
            await result
    except Exception:  # noqa: BLE001  (best effort; the body is abandoned either way)
        return


async def _granted_body_chunks(resp: Any, method: str) -> AsyncIterator[bytes]:
    """Read a successful granted response body in chunks.

    A streaming body (``getReader``) is read window by window; anything else
    is read whole.

    Args:
        resp: Granted-channel response with a 2xx status.
        method: ``GuestDatabase`` method name for the error text.

    Yields:
        Body bytes in order.

    Raises:
        PluginError: ``internal`` when the response carries no body.
    """
    stream = getattr(resp, "body", None)
    if stream is not None and hasattr(stream, "getReader"):
        chunks = _reader_chunks(stream)
        try:
            async for chunk in chunks:
                yield chunk
        finally:
            # Closing this iterator closes the reader's too, so the body is
            # cancelled and the lock released before `aclose()` returns
            # rather than whenever the inner generator is finalized.
            await chunks.aclose()
        return
    if hasattr(resp, "arrayBuffer"):
        yield bytes(await resp.arrayBuffer())
        return
    if stream is not None:
        yield bytes(await stream)
        return
    raise PluginError.from_wire("internal", f"granted {method} response missing body")


def granted_databases(
    granted: _GrantedFetcher,
    database_tokens: dict[str, str],
    *,
    signal: Any | None = None,
) -> dict[str, DatabaseBinding]:
    """Build named :class:`DatabaseBinding` objects over ``POST /db/execute``.

    Each entry of ``database_tokens`` (binding name → per-invocation grant
    token) becomes an isolated D1-shaped binding whose terminal methods
    (``first``, ``run``, ``all``, ``batch``) are async and route through the
    granted transport — the same objects the adapter installs on ``env``.

    Args:
        granted: Granted-channel fetcher (``GRANTED`` service binding).
        database_tokens: Binding name → grant token.
        signal: Optional ``AbortSignal`` forwarded to every request.

    Returns:
        Binding name → :class:`DatabaseBinding`.
    """
    from bookclerk_plugin_sdk.db_value import create_database_binding

    databases: dict[str, Any] = {}
    for name, token in (database_tokens or {}).items():
        if not isinstance(token, str) or not token:
            continue
        binding_guest = _GrantedGuestDatabase(granted, token, signal)

        def _bind(g: _GrantedGuestDatabase) -> Any:
            async def bound_execute(request: ExecuteRequest) -> ExecuteReply:
                return await g.execute(request)

            async def bound_limits() -> DbCapabilities:
                return await g.limits()

            async def bound_streamed(request: ExecuteRequest) -> bytes | AsyncIterable[bytes]:
                return await g.execute_streamed(request)

            return create_database_binding(
                bound_execute, limits=bound_limits, execute_streamed=bound_streamed
            )

        databases[name] = _bind(binding_guest)
    return databases


class _TransportGuestDatabase(GuestDatabase):
    """``GuestDatabase`` over the adapter's granted transport stub.

    The trusted adapter isolate hands the author a ``GrantedDatabaseTransport``
    for each named ``[[databases]]`` binding; its ``executeBytes`` carries the
    Cap'n ``ExecuteRequest`` / ``ExecuteResultReply`` bytes, so the author
    encodes and decodes locally and never sees the grant token.

    The transport reports channel failures inside the reply (the ``err`` arm,
    or one ``err`` stream frame), so they decode here as typed
    :class:`PluginError` with their ``details``: an exception thrown across
    Workers RPC would lose both. Anything the stub still raises (the RPC call
    itself failing) becomes ``unavailable``.
    """

    def __init__(self, transport: Any) -> None:
        self._transport = transport

    async def _call(self, method: str, *args: Any) -> Any:
        """Call one transport method, typing whatever it raises.

        Args:
            method: Transport method name (``executeBytes``, ...).
            *args: Its arguments.

        Returns:
            The method's answer.

        Raises:
            PluginError: The stub's own ``PluginError`` unchanged; any other
                exception as ``unavailable``.
        """
        try:
            return await getattr(self._transport, method)(*args)
        except PluginError:
            raise
        except Exception as err:  # noqa: BLE001 - an RPC failure has no type of its own
            raise PluginError.from_wire(
                "unavailable", f"database transport {method}: {err}"
            ) from err

    async def execute(self, request: ExecuteRequest) -> ExecuteReply:
        from bookclerk_plugin_sdk.db_value import (
            decode_execute_result_reply,
            encode_execute_request,
        )

        body = encode_execute_request(request)
        raw = await self._call("executeBytes", js(body))
        data = _bytes(raw)
        if not data:
            raise PluginError.from_wire("internal", "granted execute reply missing body")
        try:
            return decode_execute_result_reply(data)
        except PluginError:
            raise
        except Exception as err:
            raise PluginError.from_wire("internal", str(err)) from err

    async def execute_streamed(self, request: ExecuteRequest) -> bytes | AsyncIterable[bytes]:
        """The same batch through the transport's streamed methods.

        A transport that offers ``openStreamedBytes`` hands the frames over as
        the channel delivers them, so ``iterate()`` decodes them chunk by
        chunk; an older transport's ``executeStreamedBytes`` answers whole.

        Args:
            request: Statements, binds and deadline.

        Returns:
            Every ``ExecuteStreamFrame``, as a byte stream or concatenated.

        Raises:
            PluginError: ``internal`` when the reply carries no body;
                ``unsupported`` when the transport offers no streamed read.
        """
        from bookclerk_plugin_sdk.db_value import encode_execute_request

        body = encode_execute_request(request)
        if hasattr(self._transport, "openStreamedBytes"):
            raw = await self._call("openStreamedBytes", js(body))
            if hasattr(raw, "getReader"):
                return _reader_chunks(raw)
        elif hasattr(self._transport, "executeStreamedBytes"):
            raw = await self._call("executeStreamedBytes", js(body))
        else:
            # A transport that predates the streamed read: the binding falls
            # back to its inline read.
            raise PluginError.from_wire(
                "unsupported", "GuestDatabase.executeStreamed is not offered by this transport"
            )
        data = _bytes(raw)
        if not data:
            raise PluginError.from_wire("internal", "granted executeStreamed reply missing body")
        return data

    async def limits(self) -> DbCapabilities:
        """Negotiated limits through the transport's ``limitsBytes``.

        Returns:
            The adapter's ``DbCapabilities`` for this binding.
        """
        from bookclerk_plugin_sdk.db_value import decode_db_capabilities_reply

        raw = await self._call("limitsBytes")
        data = _bytes(raw)
        if not data:
            raise PluginError.from_wire("internal", "granted limits reply missing body")
        try:
            return decode_db_capabilities_reply(data)
        except PluginError:
            raise
        except Exception as err:
            raise PluginError.from_wire("internal", str(err)) from err


def transport_database(transport: Any) -> DatabaseBinding:
    """Wrap an adapter ``GrantedDatabaseTransport`` stub as a :class:`DatabaseBinding`.

    Args:
        transport: Stub exposing ``executeBytes(bytes) -> bytes``,
            ``executeStreamedBytes(bytes) -> bytes`` and ``limitsBytes() -> bytes``.

    Returns:
        D1-shaped binding whose terminal methods route through the transport.
    """
    from bookclerk_plugin_sdk.db_value import create_database_binding

    guest = _TransportGuestDatabase(transport)

    async def bound_execute(request: ExecuteRequest) -> ExecuteReply:
        return await guest.execute(request)

    async def bound_limits() -> DbCapabilities:
        return await guest.limits()

    async def bound_streamed(request: ExecuteRequest) -> bytes | AsyncIterable[bytes]:
        return await guest.execute_streamed(request)

    return create_database_binding(
        bound_execute, limits=bound_limits, execute_streamed=bound_streamed
    )
