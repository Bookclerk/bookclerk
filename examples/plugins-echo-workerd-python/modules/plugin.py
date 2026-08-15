"""Echo workerd guest — BookclerkPlugin v2 (`describe` + `integration`).

    from bookclerk_plugin_sdk.workerd import BookclerkPlugin, Integration, js

`bookclerk-workerd` injects the SDK under that module path. Native guests use
`from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest` is
removed; native guests use Rust `serve`. This module is workerd-only.
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import BookclerkPlugin, Integration, js

API_VERSION = 2
PLUGIN_ID = "echo_workerd_python"
KIND = "integration"

CLI = {
    "commands": [
        {
            "name": "ping",
            "about": "Probe echo plugin",
            "args": [
                {
                    "name": "message",
                    "long": "message",
                    "kind": "string",
                    "default": "hi",
                }
            ],
        }
    ]
}


def _get(obj, key, default=None):
    if obj is None:
        return default
    try:
        if hasattr(obj, "get"):
            val = obj.get(key)
            return default if val is None else val
    except Exception:  # noqa: BLE001
        pass
    return getattr(obj, key, default)


class EchoIntegration(Integration):
    """Integration RpcTarget: health / diagnose / onEvent."""

    def __init__(self, env=None):
        self.env = env

    async def health(self, _params=None):
        return js(
            {
                "ok": True,
                "id": PLUGIN_ID,
                "enabled": True,
                "detail": "echo workerd python plugin ready",
            }
        )

    async def diagnose(self, _params=None):
        return js({"lines": ["echo_workerd_python: ok"]})

    async def onEvent(self, event=None):
        event_type = _get(event, "type") or _get(event, "eventType")
        if event_type == "book_acquired":
            payload = _get(event, "payload") or {}
            title_id = _get(payload, "titleId") or ""
            host = getattr(self.env, "HOST", None) if self.env is not None else None
            if host is not None and hasattr(host, "notify"):
                await host.notify(
                    js(
                        {
                            "type": "plugin_log",
                            "payload": {
                                "level": "info",
                                "message": f"echo saw book_acquired titleId={title_id}",
                            },
                        }
                    )
                )
        return js({"kind": "ack"})


class Default(BookclerkPlugin):
    """Bookclerk plugin entrypoint (workerd `entrypoint = \"default\"`)."""

    async def describe(self):
        return js(
            {
                "apiVersion": API_VERSION,
                "id": PLUGIN_ID,
                "kind": KIND,
                "displayName": "Echo Integration (workerd Python)",
                "rpcFeatures": ["rpc.scalarLimits"],
                "scalarLimits": {
                    "maxScalarBytes": 262144,
                    "maxStreamWindowBytes": 1048576,
                    "maxListPage": 256,
                },
                "supportedRoles": ["integration"],
            }
        )

    def integration(self, _ctx=None):
        return EchoIntegration(getattr(self, "env", None))

    async def cliDescribe(self, _params=None):
        return js(CLI)

    async def cliInvoke(self, params=None):
        if isinstance(params, str):
            import json

            params = json.loads(params or "{}")
        command = _get(params, "command")
        if command != "ping":
            return js(
                {
                    "exitCode": 2,
                    "stderr": f"unknown command {command or ''}",
                }
            )
        args = _get(params, "args") or {}
        message = _get(args, "message")
        if not isinstance(message, str):
            message = "hi"
        return js(
            {
                "exitCode": 0,
                "stdout": f"pong: {message}\n",
                "json": {"pong": message},
            }
        )
