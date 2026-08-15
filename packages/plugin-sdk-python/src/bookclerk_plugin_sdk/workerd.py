"""Workerd BookclerkPlugin — extends Cloudflare ``WorkerEntrypoint``.

- Workerd (this module): ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``

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


PRODUCT_API_VERSION = 2
ENVELOPE_VERSION = 1
MAX_SCALAR_BYTES = 262_144
FEATURE_SCALAR_LIMITS = "rpc.scalarLimits"
FEATURE_STREAMS = "rpc.streams"
FEATURE_STORAGE_COPY = "storage.copy"


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

    async def shutdown(self):
        """Release guest resources.

        Returns:
            ``None``. The base implementation is a no-op.
        """
        return None


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
