"""Workerd BookclerkPlugin — extends Cloudflare `WorkerEntrypoint`.

Dual-stack with the native SDK:

- Workerd (this module): ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``
- Native stdio: ``from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest``

Inside a Python Workers isolate, ``bookclerk-workerd`` injects this module under
``bookclerk_plugin_sdk.workerd`` — authors do not vendor a relative filepath.
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
    """Convert a Python value to a JS object for Workers RPC / HTTP JSON."""
    return JSON.parse(json.dumps(value))


def _unsupported(method: str) -> Exception:
    err = RuntimeError(f"{method} not implemented")
    err.code = "unsupported"  # type: ignore[attr-defined]
    return err


class BookclerkPlugin(WorkerEntrypoint):
    """Branded guest base — same contract as the TS/Rust workerd SDKs."""

    async def fetch(self, _request=None):
        return Response.new(None, {"status": 404})

    async def handshake(self, _params=None):
        raise _unsupported("handshake")

    async def shutdown(self, _params=None):
        return None

    async def health(self, _params=None):
        return js({"ok": True})

    async def diagnose(self, _params=None):
        return js({"lines": []})

    async def onEvent(self, _event=None):
        raise _unsupported("onEvent")

    async def cliDescribe(self, _params=None):
        return js({"commands": []})

    async def cliInvoke(self, _params=None):
        raise _unsupported("cliInvoke")

    async def start(self, _params=None):
        raise _unsupported("start")

    async def pollEvents(self, _params=None):
        raise _unsupported("pollEvents")

    async def scanLibrary(self, _params=None):
        raise _unsupported("scanLibrary")

    async def syncListening(self, _params=None):
        raise _unsupported("syncListening")

    async def authenticateUser(self, _params=None):
        raise _unsupported("authenticateUser")

    async def login(self, _params=None):
        raise _unsupported("login")

    async def loginStart(self, _params=None):
        raise _unsupported("loginStart")

    async def loginComplete(self, _params=None):
        raise _unsupported("loginComplete")

    async def credentialsUpdate(self, _params=None):
        raise _unsupported("credentialsUpdate")

    async def scan(self, _params=None):
        raise _unsupported("scan")

    async def fetchTitle(self, _params=None):
        raise _unsupported("fetchTitle")

    async def searchCatalog(self, _params=None):
        raise _unsupported("searchCatalog")

    async def expandCandidates(self, _params=None):
        raise _unsupported("expandCandidates")

    async def purchaseHint(self, _params=None):
        raise _unsupported("purchaseHint")

    async def listDeals(self, _params=None):
        raise _unsupported("listDeals")

    async def listAccounts(self, _params=None):
        raise _unsupported("listAccounts")

    async def catalogDetail(self, _params=None):
        raise _unsupported("catalogDetail")

    async def put(self, _params=None):
        raise _unsupported("put")

    async def putFile(self, _params=None):
        raise _unsupported("putFile")

    async def get(self, _params=None):
        raise _unsupported("get")

    async def exists(self, _params=None):
        raise _unsupported("exists")

    async def list(self, _params=None):
        raise _unsupported("list")

    async def probe(self, _params=None):
        raise _unsupported("probe")

    async def copy(self, _params=None):
        raise _unsupported("copy")

    async def delete(self, _params=None):
        raise _unsupported("delete")

    async def touchFile(self, _params=None):
        raise _unsupported("touchFile")

    async def dbConnect(self, _params=None):
        raise _unsupported("dbConnect")

    async def dbPing(self, _params=None):
        raise _unsupported("dbPing")

    async def dbQuery(self, _params=None):
        raise _unsupported("dbQuery")

    async def dbExecute(self, _params=None):
        raise _unsupported("dbExecute")
