"""Workerd BookclerkPlugin — extends Cloudflare ``WorkerEntrypoint``.

- Workerd (this module): ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``

Inside a Python Workers isolate, ``bookclerk-workerd`` injects this module under
``bookclerk_plugin_sdk.workerd`` — authors do not vendor a relative filepath.
Outside the isolate (authoring / unit tests), lightweight stubs stand in for
``js`` / ``workers`` imports.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Protocol

if TYPE_CHECKING:
    from bookclerk_plugin_sdk.db_value import DatabaseBinding, ExecuteReply, ExecuteRequest

try:
    from js import JSON, Response
    from workers import WorkerEntrypoint
except ImportError:  # authoring / unit tests outside the isolate
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


def js(value):
    """Convert a Python value to a JS object for Workers RPC / HTTP JSON.

    Args:
        value: JSON-serializable Python value.

    Returns:
        A JS object produced via ``JSON.parse(json.dumps(value))`` in the isolate,
        or the equivalent parsed mapping when running under the authoring stubs.
    """
    return JSON.parse(json.dumps(value))


# Product constants come from the generated ``_abi`` projection of
# ``schema/plugin.capnp`` — re-exported here for guest convenience.
from ._abi import (  # noqa: E402  (re-export)
    FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_COPY,
    FEATURE_STREAMS,
    MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
    MAX_PLUGIN_MIGRATION_TOTAL_OPS,
    MAX_SCALAR_BYTES,
    PRODUCT_API_VERSION,
)


class PluginError(RuntimeError):
    """SDK-thrown failure. Unknown wire codes stay on ``wire_code``."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        known = {
            "invalid_params",
            "unauthorized",
            "forbidden",
            "not_found",
            "unavailable",
            "unsupported",
            "internal",
            "payload_too_large",
            "deadline_exceeded",
            "invalid_cursor",
            "cancelled",
            "conflict",
        }
        self.wire_code = code
        self.code = code if code in known else "unknown"

    @classmethod
    def from_wire(cls, code: str, message: str) -> "PluginError":
        """Build a ``PluginError`` from a wire ``code`` string.

        Args:
            code: Snake_case wire code (unknown codes are kept on ``wire_code``).
            message: Operator-facing error text.

        Returns:
            A ``PluginError`` whose ``code`` is a known variant or ``unknown``.
        """
        return cls(code, message)


class BookclerkPlugin(WorkerEntrypoint):
    """Author-facing guest. Adapter tokens are not on this env."""

    async def fetch(self, _request=None):
        """Reject HTTP fetch — workerd guests are Workers-RPC only.

        Args:
            _request: Incoming HTTP request (unused by the default stub).

        Returns:
            A 404 ``Response``.
        """
        return Response.new(None, {"status": 404})

    async def describe(self):
        """Advertise identity, features, and scalar limits.

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "describe not implemented")

    def destination(self, _context=None):
        """Return a destination capability for this invocation.

        Args:
            _context: Opaque JSON knobs (no OS paths).

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "destination not implemented")

    def source(self, _context=None):
        """Return a source capability for this invocation.

        Args:
            _context: Opaque JSON knobs (no OS paths).

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "source not implemented")

    def worker(self, _context=None):
        """Return a job handler for this invocation.

        Args:
            _context: Job id plus opaque JSON knobs (no OS paths).

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "worker not implemented")

    def content_source(self, _ctx=None):
        """Return a storefront content-source capability.

        Args:
            _ctx: Frozen invocation context.

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "contentSource not implemented")

    def contentSource(self, _ctx=None):
        """Workers RPC name for :meth:`content_source`."""
        return self.content_source(_ctx)

    def integration(self, _ctx=None):
        """Return an integration capability.

        Args:
            _ctx: Frozen invocation context.

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "integration not implemented")

    def database(self, _ctx=None):
        """Return a database factory.

        Args:
            _ctx: Frozen invocation context.

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "database not implemented")

    async def cliDescribe(self, _params=None):
        """Guest CLI schema JSON.

        Args:
            _params: Unused.

        Returns:
            Empty JS object.
        """
        return js({})

    async def cliInvoke(self, _params=None):
        """Invoke a guest CLI command.

        Args:
            _params: ``CliInvokeParams``.

        Raises:
            PluginError: With ``code="unsupported"`` on the base class.
        """
        raise PluginError.from_wire("unsupported", "cliInvoke not implemented")

    async def oidcClients(self):
        """Plugin-provided OIDC authorization-server client templates.

        Returns:
            Empty list when the guest is not a relying party.
        """
        return js([])

    async def databaseMigrations(self, _binding=""):
        """Complete ordered plugin-owned migration sequence for one binding.

        Args:
            _binding: Binding name from ``capabilities.bindings.databases``.

        Returns:
            Empty list when the binding has no plugin-owned migrations.
        """
        return js([])

    async def shutdown(self):
        """Release guest resources.

        Returns:
            ``None``. The base implementation is a no-op.
        """
        return None


class GuestDatabase:
    """Host-granted SQL transport for job plugin authors (no ``capabilities``)."""

    async def execute(self, _request: "ExecuteRequest") -> "ExecuteReply":
        """Host-mediated typed batch (``ExecuteRequest`` → ``ExecuteReply``)."""
        raise PluginError.from_wire("unsupported", "execute not implemented")

    async def close(self) -> None:
        """Close the grant."""
        return None


class AdapterDatabaseSession:
    """Host ↔ database adapter session (``capabilities`` + typed ``execute``)."""

    async def capabilities(self) -> Any:
        """Typed SQL-contract advertisement."""
        raise PluginError.from_wire("unsupported", "capabilities not implemented")

    async def execute(self, _request: "ExecuteRequest") -> "ExecuteReply":
        """Typed atomic batch (``ExecuteRequest`` → ``ExecuteReply``)."""
        raise PluginError.from_wire("unsupported", "execute not implemented")

    async def close(self) -> None:
        """Close the session."""
        return None


@dataclass
class JobContext:
    """Granted stubs for one :class:`JobHandler.handle` invocation."""

    database: "DatabaseBinding | None" = None
    """Unused in production. Jobs never inject the host library as guest SQL."""
    guest_database: GuestDatabase | None = None
    databases: "dict[str, DatabaseBinding]" = field(default_factory=dict)
    """Named plugin-owned database bindings (Workers-style).

    Declared in ``plugin.toml`` ``capabilities.bindings.databases`` and
    approved by the operator. Each binding is an isolated database — separate
    from the Bookclerk library and every other plugin — with full DML plus
    bounded idempotent DDL (``CREATE``/``DROP`` ``TABLE``/``INDEX``
    with ``IF [NOT] EXISTS``). ``ALTER`` and ``CREATE TABLE AS`` are
    refused.
    """


class JobHandler:
    """Plugin worker that handles one durable job invocation."""

    async def handle(self, _invocation: Any, _context: JobContext) -> Any:
        """Run ``invocation`` using granted capabilities until completion or cancel."""
        raise PluginError.from_wire("unsupported", "handle not implemented")


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

    async def execute(self, request: "ExecuteRequest") -> "ExecuteReply":
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


def granted_job_context(
    granted: _GrantedFetcher,
    grant_token: str,
    *,
    signal: Any | None = None,
    database_tokens: "dict[str, str] | None" = None,
) -> JobContext:
    """Build a :class:`JobContext` with host-mediated SQL over ``POST /db/execute``.

    The returned :attr:`JobContext.database` is a :class:`DatabaseBinding`
    whose terminal methods (``first``, ``run``, ``all``, ``batch``) are async
    and route through the granted transport. ``database_tokens`` maps named
    plugin database bindings to their per-invocation grant tokens; each entry
    becomes an isolated :class:`DatabaseBinding` on
    :attr:`JobContext.databases`.
    """
    from bookclerk_plugin_sdk.db_value import (
        ExecuteReply,
        ExecuteRequest,
        create_database_binding,
    )

    guest = _GrantedGuestDatabase(granted, grant_token, signal)

    async def execute(request: ExecuteRequest) -> ExecuteReply:
        return await guest.execute(request)

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

    return JobContext(
        database=create_database_binding(execute),
        guest_database=guest,
        databases=databases,
    )


class Integration:
    """Integration role returned by ``BookclerkPlugin.integration``.

    Python Workers treat the returned object as an RpcTarget. Override
    ``health``, ``diagnose``, and ``onEvent``.
    """

    async def health(self, _params=None):
        """Report integration liveness.

        Args:
            _params: Unused.

        Returns:
            JS object with ``ok: True``.
        """
        return js({"ok": True})

    async def diagnose(self, _params=None):
        """Return diagnostic lines.

        Args:
            _params: Unused.

        Returns:
            JS object with a ``lines`` list.
        """
        return js({"lines": []})

    async def onEvent(self, _event=None):
        """Handle a host-pushed domain event.

        Args:
            _event: Domain event payload.

        Returns:
            JS object ``{"kind": "ack"}``.
        """
        return js({"kind": "ack"})
