"""Workerd BookclerkPlugin — extends Cloudflare ``WorkerEntrypoint``.

Dual-stack with the native SDK:

- Workerd (this module): ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``
- Native stdio: ``from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest``

Inside a Python Workers isolate, ``bookclerk-workerd`` injects this module under
``bookclerk_plugin_sdk.workerd`` — authors do not vendor a relative filepath.
Outside the isolate (authoring / unit tests), lightweight stubs stand in for
``js`` / ``workers`` imports.
"""

from __future__ import annotations

import json

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


def _unsupported(method: str) -> Exception:
    err = RuntimeError(f"{method} not implemented")
    err.code = "unsupported"  # type: ignore[attr-defined]
    return err


class BookclerkPlugin(WorkerEntrypoint):
    """Branded guest base — same contract as the TS/Rust workerd SDKs.

    Override the async methods your ``plugin.toml`` advertises. Default optional
    methods raise ``RuntimeError`` with ``code = "unsupported"``. Binding
    ``env`` comes from ``WorkerEntrypoint``.

    Examples:
        >>> # In modules/plugin.py under a workerd plugin:
        >>> # from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js
        >>> # class Default(BookclerkPlugin):
        >>> #     async def handshake(self, params=None):
        >>> #         return js({"apiVersion": 1, "id": "echo", "kind": "source",
        >>> #                    "capabilities": ["health"]})
    """

    async def fetch(self, _request=None):
        """Handle HTTP fetch (defaults to 404; RPC uses Workers methods).

        Args:
            _request: Incoming HTTP request (unused by the default stub).

        Returns:
            A 404 ``Response`` so accidental HTTP hits do not expose RPC.
        """
        return Response.new(None, {"status": 404})

    async def handshake(self, _params=None):
        """Run the guest handshake against the host bridge.

        Args:
            _params: Negotiated install identity and host config.

        Returns:
            Handshake result including ``apiVersion``, plugin ``id``, and ``kind``.

        Raises:
            RuntimeError: With ``code="unsupported"`` on the base class.
        """
        raise _unsupported("handshake")

    async def shutdown(self, _params=None):
        """Shut down the guest cleanly.

        Args:
            _params: Optional shutdown parameters from the host.

        Returns:
            ``None``. The base implementation is a no-op.
        """
        return None

    async def health(self, _params=None):
        """Report guest liveness.

        Args:
            _params: Optional health parameters from the host.

        Returns:
            JS object with at least ``ok: True``.
        """
        return js({"ok": True})

    async def diagnose(self, _params=None):
        """Return diagnostic lines for operator tooling.

        Args:
            _params: Optional diagnose parameters from the host.

        Returns:
            JS object with a ``lines`` list (empty by default).
        """
        return js({"lines": []})

    async def onEvent(self, _event=None):
        """Handle a host-pushed event.

        Args:
            _event: Event payload from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("onEvent")

    async def cliDescribe(self, _params=None):
        """Describe CLI commands exposed by this guest.

        Args:
            _params: Optional describe parameters from the host.

        Returns:
            JS object with a ``commands`` list (empty by default).
        """
        return js({"commands": []})

    async def cliInvoke(self, _params=None):
        """Invoke a guest CLI command.

        Args:
            _params: Command name and argument map from the host.

        Returns:
            Command result (typically ``exitCode``, ``stdout``, ``stderr``).

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("cliInvoke")

    async def start(self, _params=None):
        """Start long-running guest work (integration plugins).

        Args:
            _params: Host start parameters.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("start")

    async def pollEvents(self, _params=None):
        """Poll for guest-emitted events.

        Args:
            _params: Optional poll parameters from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("pollEvents")

    async def scanLibrary(self, _params=None):
        """Scan the connected library for titles.

        Args:
            _params: Scan options from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("scanLibrary")

    async def syncListening(self, _params=None):
        """Sync listening progress with the storefront.

        Args:
            _params: Optional sync parameters from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("syncListening")

    async def authenticateUser(self, _params=None):
        """Authenticate a library user via the guest.

        Args:
            _params: Authentication parameters from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("authenticateUser")

    async def login(self, _params=None):
        """Perform a synchronous store login.

        Args:
            _params: Login credentials / options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("login")

    async def loginStart(self, _params=None):
        """Begin an interactive / OAuth login flow.

        Args:
            _params: Login-start parameters from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("loginStart")

    async def loginComplete(self, _params=None):
        """Complete an interactive / OAuth login flow.

        Args:
            _params: Completion parameters (callback payload, codes, etc.).

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("loginComplete")

    async def credentialsUpdate(self, _params=None):
        """Update stored credentials for an account.

        Args:
            _params: Credential update payload.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("credentialsUpdate")

    async def scan(self, _params=None):
        """Scan a source account for titles.

        Args:
            _params: Scan options from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("scan")

    async def fetchTitle(self, _params=None):
        """Fetch / acquire a single title.

        Args:
            _params: Title identity and acquire options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("fetchTitle")

    async def searchCatalog(self, _params=None):
        """Search the storefront catalog.

        Args:
            _params: Search query and filters.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("searchCatalog")

    async def expandCandidates(self, _params=None):
        """Expand discover candidates for a work.

        Args:
            _params: Candidate expansion parameters.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("expandCandidates")

    async def purchaseHint(self, _params=None):
        """Return a purchase hint / deep link.

        Args:
            _params: Title identity for the hint.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("purchaseHint")

    async def listDeals(self, _params=None):
        """List storefront deals.

        Args:
            _params: Deal listing options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("listDeals")

    async def listAccounts(self, _params=None):
        """List accounts known to the guest.

        Args:
            _params: Account listing options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("listAccounts")

    async def catalogDetail(self, _params=None):
        """Fetch catalog detail for a title.

        Args:
            _params: Catalog identity parameters.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("catalogDetail")

    async def put(self, _params=None):
        """Write bytes to a destination.

        Args:
            _params: Destination path and payload.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("put")

    async def putFile(self, _params=None):
        """Write a local file to a destination.

        Args:
            _params: Source file and destination path.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("putFile")

    async def get(self, _params=None):
        """Read bytes from a destination.

        Args:
            _params: Destination path to read.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("get")

    async def exists(self, _params=None):
        """Test whether a destination object exists.

        Args:
            _params: Destination path to probe.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("exists")

    async def list(self, _params=None):
        """List objects under a destination prefix.

        Args:
            _params: Listing prefix / options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("list")

    async def probe(self, _params=None):
        """Probe destination connectivity / capabilities.

        Args:
            _params: Probe options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("probe")

    async def copy(self, _params=None):
        """Copy an object within a destination.

        Args:
            _params: Source and destination paths.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("copy")

    async def delete(self, _params=None):
        """Delete an object from a destination.

        Args:
            _params: Destination path to delete.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("delete")

    async def touchFile(self, _params=None):
        """Touch / update metadata on a destination object.

        Args:
            _params: Path and touch options.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("touchFile")

    async def dbConnect(self, _params=None):
        """Open a database session.

        Args:
            _params: Connection parameters from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("dbConnect")

    async def dbPing(self, _params=None):
        """Ping the database backend.

        Args:
            _params: Optional ping parameters from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("dbPing")

    async def dbQuery(self, _params=None):
        """Run a read query against the database.

        Args:
            _params: SQL / query parameters.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("dbQuery")

    async def dbExecute(self, _params=None):
        """Execute a write statement against the database.

        Args:
            _params: SQL / execute parameters.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        raise _unsupported("dbExecute")
