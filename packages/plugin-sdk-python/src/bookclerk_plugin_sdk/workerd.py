"""Workerd author runtime — ``BookclerkEntrypoint`` and the named entrypoints.

Python mirror of ``@bookclerk/plugin-sdk/workerd`` (``embed/bookclerk_plugin.js``)
for ``api_version = 3`` guests running as Python Workers:

- ``from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, js``

The default export is a module-level ``Default`` class extending
:class:`BookclerkEntrypoint` with optional ``event(batch)`` / ``job(job)``
triggers. Named entrypoints from ``plugin.toml`` ``entrypoints`` are module-level
classes named after the wire name (``Storefront``, ``Storage``, ``RemoteLibrary``,
``DatabaseAdapter``, ``Cli``, ``Oidc``) extending the matching ``*Entrypoint``
base. The launcher's adapter isolate reaches them only through the
``bookclerk*`` dispatch methods; authors neither call nor override those.

Inside a Python Workers isolate, ``bookclerk-workerd`` injects this module under
``bookclerk_plugin_sdk.workerd`` — authors do not vendor a relative filepath.
Outside the isolate (authoring / unit tests), lightweight stubs stand in for
``js`` / ``workers`` imports and :func:`js` returns plain Python values.
"""

from __future__ import annotations

import inspect
import json
import time
from datetime import datetime
from typing import TYPE_CHECKING, Any, Protocol

if TYPE_CHECKING:
    from bookclerk_plugin_sdk import abi
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
    FEATURE_STORAGE_COPY,
    FEATURE_STREAMS,
    MAX_CHECKPOINT_BYTES,
    MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
    MAX_PLUGIN_MIGRATION_TOTAL_OPS,
    MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES,
    PLUGIN_ERROR_CODES,
    PRODUCT_API_VERSION,
)

__all__ = [
    "FEATURE_SCALAR_LIMITS",
    "FEATURE_STORAGE_COPY",
    "FEATURE_STREAMS",
    "MAX_CHECKPOINT_BYTES",
    "MAX_LIST_PAGE",
    "MAX_PLUGIN_MIGRATION_OPS",
    "MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES",
    "MAX_PLUGIN_MIGRATION_TOTAL_OPS",
    "MAX_SCALAR_BYTES",
    "MAX_STREAM_WINDOW_BYTES",
    "PRODUCT_API_VERSION",
    "AdapterDatabaseSession",
    "BookclerkEntrypoint",
    "CliEntrypoint",
    "DatabaseAdapterEntrypoint",
    "EventBatch",
    "EventMessage",
    "GuestDatabase",
    "JobController",
    "NamedEntrypoint",
    "OidcEntrypoint",
    "PluginError",
    "RemoteLibraryEntrypoint",
    "StorageEntrypoint",
    "StorefrontEntrypoint",
    "cli_args",
    "decode_extensible_config",
    "event_batch_results",
    "granted_databases",
    "job_outcome_for",
    "js",
    "json_payload",
    "py",
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


def py(value: Any) -> Any:
    """Convert a JS value delivered over Workers RPC into plain Python.

    ``JsProxy`` objects are converted with ``to_py()`` (``Uint8Array`` →
    ``bytes``); Python values pass through unchanged.

    Args:
        value: A JsProxy or Python value.

    Returns:
        A plain Python value (``dict`` / ``list`` / ``bytes`` / scalars).
    """
    if value is None or isinstance(value, (str, int, float, bool, bytes, dict, list)):
        return value
    to_py = getattr(value, "to_py", None)
    if callable(to_py):
        converted = to_py()
        if isinstance(converted, memoryview):
            return bytes(converted)
        return converted
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


class PluginError(RuntimeError):
    """SDK-thrown failure. Unknown wire codes stay on ``wire_code``."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.wire_code = code
        self.code = code if code in PLUGIN_ERROR_CODES else "unknown"

    @classmethod
    def from_wire(cls, code: str, message: str) -> PluginError:
        """Build a ``PluginError`` from a wire ``code`` string.

        Args:
            code: Snake_case wire code (unknown codes are kept on ``wire_code``).
            message: Operator-facing error text.

        Returns:
            A ``PluginError`` whose ``code`` is a known variant or ``unknown``.
        """
        return cls(code, message)


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
    storage = _get(ctx, "storage")
    if storage is not None:
        granted["WORK_FS"] = storage
    events = _get(ctx, "events")
    if events is not None:
        granted["EVENTS"] = events
    databases = _get(ctx, "databases")
    if databases is not None:
        for entry in databases:
            name = _get(entry, "name")
            database = _get(entry, "database")
            if isinstance(name, str) and database is not None:
                granted[name] = database
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
            {"kind": "retryable", "message": str(reason or ""), "retryAfterUnixMs": _unix_ms(retry_at)}
        )

    def cancel(self) -> None:
        """Host-side cancellation observed (adapter-internal)."""
        self.cancelled = True


def job_outcome_for(job: JobController, returned: Any, failed: BaseException | None) -> dict[str, Any]:
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


# ---------------------------------------------------------------------------
# Author base classes
# ---------------------------------------------------------------------------


class BookclerkEntrypoint(WorkerEntrypoint):
    """Default-export base for ``api_version = 3`` Python Workers guests.

    Optional trigger methods: ``event(batch)`` (for ``[[events.consumers]]``)
    and ``job(job)`` (for ``[triggers] jobs``); optional ``describe()``
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
    "listDeals",
    "catalogDetail",
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
        """Enumerate owned titles (``ScanParams`` → ``ScanSummary``)."""
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
        """Purchase link / price hint (``PurchaseHintParams`` → ``PurchaseHint``)."""
        raise _unsupported("purchaseHint")

    async def listDeals(self, _params: abi.ListDealsParams):
        """Current storefront deals (``ListDealsParams`` → list of ``CatalogHit``)."""
        raise _unsupported("listDeals")

    async def catalogDetail(self, _params: abi.CatalogDetailParams):
        """Full catalog record (``CatalogDetailParams`` → ``CatalogHit``)."""
        raise _unsupported("catalogDetail")

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


class DatabaseAdapterEntrypoint(NamedEntrypoint):
    """``databaseAdapter`` entrypoint: opens typed SQL sessions for the host library."""

    bookclerk_methods = ("openSession",)

    async def openSession(self):
        """Open an :class:`AdapterDatabaseSession`."""
        raise _unsupported("openSession")


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


# ---------------------------------------------------------------------------
# Database capabilities
# ---------------------------------------------------------------------------


class GuestDatabase:
    """Host-granted SQL transport behind a named ``[[databases]]`` binding."""

    async def execute(self, _request: ExecuteRequest) -> ExecuteReply:
        """Host-mediated typed batch (``ExecuteRequest`` → ``ExecuteReply``)."""
        raise _unsupported("execute")

    async def close(self) -> None:
        """Close the grant."""
        return None


class AdapterDatabaseSession:
    """Host ↔ database adapter session (``capabilities`` + typed ``execute``)."""

    async def capabilities(self) -> Any:
        """Typed SQL-contract advertisement."""
        raise _unsupported("capabilities")

    async def execute(self, _request: ExecuteRequest) -> ExecuteReply:
        """Typed atomic batch (``ExecuteRequest`` → ``ExecuteReply``)."""
        raise _unsupported("execute")

    async def close(self) -> None:
        """Close the session."""
        return None


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
        status = getattr(resp, "status", None)
        if status is not None and int(status) >= 400:
            text = await resp.text() if hasattr(resp, "text") else str(resp)
            raise PluginError.from_wire("unavailable", f"database grant: {status} {text}")
        if hasattr(resp, "arrayBuffer"):
            raw = await resp.arrayBuffer()
            data = bytes(raw) if not isinstance(raw, (bytes, bytearray)) else bytes(raw)
        elif hasattr(resp, "body"):
            data = bytes(await resp.body)
        else:
            raise PluginError.from_wire("internal", "granted execute response missing body")
        try:
            return decode_execute_result_reply(data)
        except PluginError:
            raise
        except Exception as err:
            raise PluginError.from_wire("internal", str(err)) from err


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
    from bookclerk_plugin_sdk.db_value import ExecuteReply, ExecuteRequest, create_database_binding

    databases: dict[str, Any] = {}
    for name, token in (database_tokens or {}).items():
        if not isinstance(token, str) or not token:
            continue
        binding_guest = _GrantedGuestDatabase(granted, token, signal)

        def _bind(g: _GrantedGuestDatabase) -> Any:
            async def bound_execute(request: ExecuteRequest) -> ExecuteReply:
                return await g.execute(request)

            return create_database_binding(bound_execute)

        databases[name] = _bind(binding_guest)
    return databases
