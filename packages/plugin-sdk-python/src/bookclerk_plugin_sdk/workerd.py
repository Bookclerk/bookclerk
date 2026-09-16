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
    from bookclerk_plugin_sdk import abi
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


def _unsupported(method: str) -> PluginError:
    """Build the ``unsupported`` error the role base classes raise.

    Args:
        method: Wire method name.

    Returns:
        A ``PluginError`` with ``code="unsupported"``.
    """
    return PluginError.from_wire("unsupported", f"{method} not implemented")


class ContentSource:
    """Storefront role returned by ``BookclerkPlugin.content_source``.

    Every method mirrors the Cap'n ``ContentSource`` interface; parameters and
    return values are the generated :mod:`bookclerk_plugin_sdk.abi` TypedDicts
    (``LoginParams`` → ``LoginResult``, ``ScanParams`` → ``ScanSummary``, …).
    Python Workers treat the returned object as an RpcTarget. Override only the
    operations the storefront supports; the rest raise ``unsupported``.
    """

    async def login(self, _params: "abi.LoginParams"):
        """Password or one-shot OAuth login.

        Args:
            _params: ``LoginParams``.

        Returns:
            ``LoginResult`` — account identity plus opaque credentials.
        """
        raise _unsupported("login")

    async def scan(self, _params: "abi.ScanParams"):
        """Enumerate owned titles for one account.

        Args:
            _params: ``ScanParams``.

        Returns:
            ``ScanSummary``.
        """
        raise _unsupported("scan")

    async def fetchTitle(self, _params: "abi.FetchTitleParams"):
        """Fetch one title's media and metadata.

        Args:
            _params: ``FetchTitleParams``.

        Returns:
            ``PlainFetch`` — plain parts plus artifacts.
        """
        raise _unsupported("fetchTitle")

    async def listAccounts(self):
        """List accounts known to this storefront.

        Returns:
            List of ``SourceAccount``.
        """
        return js([])

    async def loginStart(self, _params: "abi.LoginParams"):
        """Begin an interactive OAuth login.

        Args:
            _params: ``LoginParams``.

        Returns:
            ``LoginStartResult`` — session id plus authorization URL.
        """
        raise _unsupported("loginStart")

    async def loginComplete(self, _params: "abi.LoginCompleteParams"):
        """Finish an interactive OAuth login started by ``loginStart``.

        Args:
            _params: ``LoginCompleteParams``.

        Returns:
            ``LoginResult``.
        """
        raise _unsupported("loginComplete")

    async def searchCatalog(self, _params: "abi.SearchCatalogParams"):
        """Free-text storefront catalog search.

        Args:
            _params: ``SearchCatalogParams``.

        Returns:
            List of ``CatalogHit``.
        """
        raise _unsupported("searchCatalog")

    async def expandCandidates(self, _params: "abi.ExpandCandidatesParams"):
        """Related-title expansion from a seed title.

        Args:
            _params: ``ExpandCandidatesParams``.

        Returns:
            List of ``CatalogHit``.
        """
        raise _unsupported("expandCandidates")

    async def purchaseHint(self, _params: "abi.PurchaseHintParams"):
        """Purchase link / price hint for one title.

        Args:
            _params: ``PurchaseHintParams``.

        Returns:
            ``PurchaseHint``.
        """
        raise _unsupported("purchaseHint")

    async def listDeals(self, _params: "abi.ListDealsParams"):
        """Current storefront deals.

        Args:
            _params: ``ListDealsParams``.

        Returns:
            List of ``CatalogHit``.
        """
        raise _unsupported("listDeals")

    async def health(self):
        """Liveness / readiness probe.

        Returns:
            ``HealthOk`` (empty struct).
        """
        return js({})

    async def diagnose(self):
        """Human-readable diagnostic lines.

        Returns:
            List of strings.
        """
        return js([])

    async def catalogDetail(self, _params: "abi.CatalogDetailParams"):
        """Full catalog record for one product.

        Args:
            _params: ``CatalogDetailParams``.

        Returns:
            ``CatalogHit``.
        """
        raise _unsupported("catalogDetail")


class Integration:
    """Integration role returned by ``BookclerkPlugin.integration``.

    Every method mirrors the Cap'n ``Integration`` interface with generated
    :mod:`bookclerk_plugin_sdk.abi` TypedDicts. Python Workers treat the
    returned object as an RpcTarget. Override the operations the integration
    supports; the lifecycle and probe methods default to no-ops.
    """

    async def health(self):
        """Report integration liveness.

        Returns:
            ``HealthOk`` (empty struct).
        """
        return js({})

    async def onEvent(self, _event: "abi.DomainEvent"):
        """Handle a host-pushed domain event (at-least-once; must be idempotent).

        Args:
            _event: ``DomainEvent`` envelope.

        Returns:
            ``EventResult`` union projected as ``{"kind": "ack" | "retry" |
            "reject" | "deadLetter" | "suspended", ...}``; the default
            acknowledges.
        """
        return js({"kind": "ack"})

    async def start(self):
        """Start background work after the host has granted bindings.

        Returns:
            ``None``.
        """
        return None

    async def stop(self):
        """Stop background work; the host may drop the capability afterwards.

        Returns:
            ``None``.
        """
        return None

    async def diagnose(self):
        """Return diagnostic lines.

        Returns:
            List of strings.
        """
        return js([])

    async def scanLibrary(self, _params: "abi.ScanLibraryParams"):
        """Re-sync the remote library.

        Args:
            _params: ``ScanLibraryParams`` (full-rescan flag).

        Returns:
            ``None``.
        """
        raise _unsupported("scanLibrary")

    async def syncListening(self):
        """Push / pull listening progress.

        Returns:
            ``SyncListeningResult``.
        """
        raise _unsupported("syncListening")

    async def authenticateUser(self, _params: "abi.AuthenticateUserParams"):
        """Verify remote credentials on behalf of the host.

        Args:
            _params: ``AuthenticateUserParams`` (username and password).

        Returns:
            ``ExternalUser``.
        """
        raise _unsupported("authenticateUser")

    async def pollEvents(self):
        """Drain events the remote side produced since the last poll.

        Returns:
            ``EventPollResult`` (``users`` list).
        """
        return js({"users": []})
