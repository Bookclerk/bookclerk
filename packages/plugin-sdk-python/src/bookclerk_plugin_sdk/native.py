"""Native stdio Workers RPC — BookclerkPlugin + BookclerkPluginGuest.

Dual-stack with workerd:

- Native:  ``from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest``
- Workerd: ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``

``BookclerkPluginGuest.serve`` frames stdin/stdout; authors subclass ``BookclerkPlugin``.
"""

from __future__ import annotations

import json
import sys
from typing import Any, Callable, Mapping, MutableMapping


class BookclerkPlugin:
    """Branded native guest base — same method surface as workerd BookclerkPlugin."""

    def handshake(self, params: Mapping[str, Any]) -> Mapping[str, Any]:
        raise NotImplementedError("handshake")

    def shutdown(self) -> None:
        return None

    def health(self) -> Mapping[str, Any]:
        return {"ok": True}

    def diagnose(self) -> Mapping[str, Any]:
        return {"lines": []}

    def on_event(self, _event: Mapping[str, Any]) -> None:
        err = RuntimeError("onEvent not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    # CamelCase aliases match Workers RPC method names / workerd overrides.
    def onEvent(self, event: Mapping[str, Any]) -> None:  # noqa: N802
        return self.on_event(event)

    def cli_describe(self) -> Mapping[str, Any]:
        return {"commands": []}

    def cliDescribe(self) -> Mapping[str, Any]:  # noqa: N802
        return self.cli_describe()

    def cli_invoke(self, _params: Mapping[str, Any]) -> Mapping[str, Any]:
        err = RuntimeError("cliInvoke not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def cliInvoke(self, params: Mapping[str, Any]) -> Mapping[str, Any]:  # noqa: N802
        return self.cli_invoke(params)

    def start(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("start not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def start(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.start(params)

    def poll_events(self) -> Any:
        err = RuntimeError("pollEvents not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def pollEvents(self) -> Any:  # noqa: N802
        return self.poll_events()

    def scan_library(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("scanLibrary not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def scanLibrary(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.scan_library(params)

    def sync_listening(self) -> Any:
        err = RuntimeError("syncListening not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def syncListening(self) -> Any:  # noqa: N802
        return self.sync_listening()

    def authenticate_user(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("authenticateUser not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def authenticateUser(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.authenticate_user(params)

    def login(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("login not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def login(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.login(params)

    def login_start(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("loginStart not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def loginStart(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.login_start(params)

    def login_complete(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("loginComplete not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def loginComplete(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.login_complete(params)

    def credentials_update(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("credentialsUpdate not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def credentialsUpdate(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.credentials_update(params)

    def scan(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("scan not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def scan(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.scan(params)

    def fetch_title(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("fetchTitle not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def fetchTitle(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.fetch_title(params)

    def search_catalog(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("searchCatalog not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def searchCatalog(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.search_catalog(params)

    def expand_candidates(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("expandCandidates not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def expandCandidates(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.expand_candidates(params)

    def purchase_hint(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("purchaseHint not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def purchaseHint(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.purchase_hint(params)

    def list_deals(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("listDeals not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def listDeals(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.list_deals(params)

    def list_accounts(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("listAccounts not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def listAccounts(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.list_accounts(params)

    def catalog_detail(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("catalogDetail not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def catalogDetail(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.catalog_detail(params)

    def put(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("put not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def put(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.put(params)

    def put_file(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("putFile not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def putFile(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.put_file(params)

    def get(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("get not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def get(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.get(params)

    def exists(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("exists not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def exists(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.exists(params)

    def list(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("list not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def list(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.list(params)

    def probe(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("probe not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def probe(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.probe(params)

    def copy(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("copy not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def copy(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.copy(params)

    def delete(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("delete not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def delete(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.delete(params)

    def touch_file(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("touchFile not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def touchFile(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.touch_file(params)

    def db_connect(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("dbConnect not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbConnect(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.db_connect(params)

    def db_ping(self) -> Any:
        err = RuntimeError("dbPing not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbPing(self) -> Any:  # noqa: N802
        return self.db_ping()

    def db_query(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("dbQuery not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbQuery(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.db_query(params)

    def db_execute(self, _params: Mapping[str, Any]) -> Any:
        err = RuntimeError("dbExecute not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbExecute(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        return self.db_execute(params)


class BookclerkPluginGuest:
    """Native guest runner — hosts a BookclerkPlugin on stdin/stdout (Workers RPC)."""

    @staticmethod
    def serve(
        plugin: Any = None,
        *,
        handlers: Mapping[str, Callable[[Any], Any]] | None = None,
    ) -> None:
        dispatch = dict(handlers) if handlers is not None else _dispatch_from_plugin(plugin)
        for raw in sys.stdin:
            line = raw.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError as err:
                _write({"id": None, "error": {"code": "internal", "message": f"invalid JSON: {err}"}})
                continue
            req_id = req.get("id")
            method = req.get("method") or ""
            params = req.get("params")
            try:
                if method not in dispatch:
                    err = RuntimeError(f"unsupported method: {method}")
                    err.code = "unsupported"  # type: ignore[attr-defined]
                    raise err
                result = dispatch[method](params)
                _write({"id": req_id, "result": result})
                if method == "shutdown":
                    return
            except Exception as err:  # noqa: BLE001 — RPC boundary
                code = getattr(err, "code", "internal")
                _write({"id": req_id, "error": {"code": code, "message": str(err)}})


def _dispatch_from_plugin(plugin: Any) -> dict[str, Callable[[Any], Any]]:
    def invoke(name: str, *alts: str) -> Callable[[Any], Any] | None:
        for n in (name, *alts):
            if hasattr(plugin, n):
                return getattr(plugin, n)
        return None

    hs = invoke("handshake")
    if hs is None:
        raise TypeError("plugin must implement handshake (subclass BookclerkPlugin)")

    def on_shutdown(_p: Any) -> None:
        fn = invoke("shutdown")
        if fn:
            fn()
        return None

    def on_health(_p: Any) -> Mapping[str, Any]:
        fn = invoke("health")
        return fn() if fn else {"ok": True}

    def on_diagnose(_p: Any) -> Mapping[str, Any]:
        fn = invoke("diagnose")
        return fn() if fn else {"lines": []}

    def on_event(p: Any) -> Mapping[str, Any]:
        fn = invoke("onEvent", "on_event")
        if not fn:
            err = RuntimeError("onEvent not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        fn(p or {})
        return {"ok": True}

    def on_cli_describe(_p: Any) -> Mapping[str, Any]:
        fn = invoke("cliDescribe", "cli_describe")
        return fn() if fn else {"commands": []}

    def on_cli_invoke(p: Any) -> Mapping[str, Any]:
        fn = invoke("cliInvoke", "cli_invoke")
        if not fn:
            err = RuntimeError("cliInvoke not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_start(p: Any) -> Any:
        fn = invoke("start", "start")
        if not fn:
            err = RuntimeError("start not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_poll_events(_p: Any) -> Any:
        fn = invoke("pollEvents", "poll_events")
        if not fn:
            err = RuntimeError("pollEvents not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn()

    def _on_scan_library(p: Any) -> Any:
        fn = invoke("scanLibrary", "scan_library")
        if not fn:
            err = RuntimeError("scanLibrary not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_sync_listening(_p: Any) -> Any:
        fn = invoke("syncListening", "sync_listening")
        if not fn:
            err = RuntimeError("syncListening not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn()

    def _on_authenticate_user(p: Any) -> Any:
        fn = invoke("authenticateUser", "authenticate_user")
        if not fn:
            err = RuntimeError("authenticateUser not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_login(p: Any) -> Any:
        fn = invoke("login", "login")
        if not fn:
            err = RuntimeError("login not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_login_start(p: Any) -> Any:
        fn = invoke("loginStart", "login_start")
        if not fn:
            err = RuntimeError("loginStart not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_login_complete(p: Any) -> Any:
        fn = invoke("loginComplete", "login_complete")
        if not fn:
            err = RuntimeError("loginComplete not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_credentials_update(p: Any) -> Any:
        fn = invoke("credentialsUpdate", "credentials_update")
        if not fn:
            err = RuntimeError("credentialsUpdate not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_scan(p: Any) -> Any:
        fn = invoke("scan", "scan")
        if not fn:
            err = RuntimeError("scan not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_fetch_title(p: Any) -> Any:
        fn = invoke("fetchTitle", "fetch_title")
        if not fn:
            err = RuntimeError("fetchTitle not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_search_catalog(p: Any) -> Any:
        fn = invoke("searchCatalog", "search_catalog")
        if not fn:
            err = RuntimeError("searchCatalog not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_expand_candidates(p: Any) -> Any:
        fn = invoke("expandCandidates", "expand_candidates")
        if not fn:
            err = RuntimeError("expandCandidates not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_purchase_hint(p: Any) -> Any:
        fn = invoke("purchaseHint", "purchase_hint")
        if not fn:
            err = RuntimeError("purchaseHint not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_list_deals(p: Any) -> Any:
        fn = invoke("listDeals", "list_deals")
        if not fn:
            err = RuntimeError("listDeals not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_list_accounts(p: Any) -> Any:
        fn = invoke("listAccounts", "list_accounts")
        if not fn:
            err = RuntimeError("listAccounts not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_catalog_detail(p: Any) -> Any:
        fn = invoke("catalogDetail", "catalog_detail")
        if not fn:
            err = RuntimeError("catalogDetail not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_put(p: Any) -> Any:
        fn = invoke("put", "put")
        if not fn:
            err = RuntimeError("put not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_put_file(p: Any) -> Any:
        fn = invoke("putFile", "put_file")
        if not fn:
            err = RuntimeError("putFile not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_get(p: Any) -> Any:
        fn = invoke("get", "get")
        if not fn:
            err = RuntimeError("get not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_exists(p: Any) -> Any:
        fn = invoke("exists", "exists")
        if not fn:
            err = RuntimeError("exists not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_list(p: Any) -> Any:
        fn = invoke("list", "list")
        if not fn:
            err = RuntimeError("list not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_probe(p: Any) -> Any:
        fn = invoke("probe", "probe")
        if not fn:
            err = RuntimeError("probe not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_copy(p: Any) -> Any:
        fn = invoke("copy", "copy")
        if not fn:
            err = RuntimeError("copy not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_delete(p: Any) -> Any:
        fn = invoke("delete", "delete")
        if not fn:
            err = RuntimeError("delete not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_touch_file(p: Any) -> Any:
        fn = invoke("touchFile", "touch_file")
        if not fn:
            err = RuntimeError("touchFile not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_db_connect(p: Any) -> Any:
        fn = invoke("dbConnect", "db_connect")
        if not fn:
            err = RuntimeError("dbConnect not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_db_ping(_p: Any) -> Any:
        fn = invoke("dbPing", "db_ping")
        if not fn:
            err = RuntimeError("dbPing not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn()

    def _on_db_query(p: Any) -> Any:
        fn = invoke("dbQuery", "db_query")
        if not fn:
            err = RuntimeError("dbQuery not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_db_execute(p: Any) -> Any:
        fn = invoke("dbExecute", "db_execute")
        if not fn:
            err = RuntimeError("dbExecute not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    return {
        "handshake": lambda p: hs(p or {}),
        "shutdown": on_shutdown,
        "health": on_health,
        "diagnose": on_diagnose,
        "onEvent": on_event,
        "cliDescribe": on_cli_describe,
        "cliInvoke": on_cli_invoke,
        "start": _on_start,
        "pollEvents": _on_poll_events,
        "scanLibrary": _on_scan_library,
        "syncListening": _on_sync_listening,
        "authenticateUser": _on_authenticate_user,
        "login": _on_login,
        "loginStart": _on_login_start,
        "loginComplete": _on_login_complete,
        "credentialsUpdate": _on_credentials_update,
        "scan": _on_scan,
        "fetchTitle": _on_fetch_title,
        "searchCatalog": _on_search_catalog,
        "expandCandidates": _on_expand_candidates,
        "purchaseHint": _on_purchase_hint,
        "listDeals": _on_list_deals,
        "listAccounts": _on_list_accounts,
        "catalogDetail": _on_catalog_detail,
        "put": _on_put,
        "putFile": _on_put_file,
        "get": _on_get,
        "exists": _on_exists,
        "list": _on_list,
        "probe": _on_probe,
        "copy": _on_copy,
        "delete": _on_delete,
        "touchFile": _on_touch_file,
        "dbConnect": _on_db_connect,
        "dbPing": _on_db_ping,
        "dbQuery": _on_db_query,
        "dbExecute": _on_db_execute,
    }


def _write(obj: MutableMapping[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
