"""Echo workerd guest — package import for BookclerkPlugin.

    from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

`bookclerk-workerd` injects the SDK under that module path. Native guests use
`from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest` instead.
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

API_VERSION = 1
PLUGIN_ID = "echo-workerd-python"
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


class Default(BookclerkPlugin):
    """Bookclerk plugin entrypoint (workerd `entrypoint = \"default\"`)."""

    async def handshake(self, _params=None):
        return js(
            {
                "apiVersion": API_VERSION,
                "id": PLUGIN_ID,
                "kind": KIND,
                "displayName": "Echo Integration (workerd Python)",
                "capabilities": ["health", "diagnose", "onEvent", "cli"],
                "cli": CLI,
            }
        )

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
        return js({"lines": ["echo-workerd-python: ok"]})

    async def onEvent(self, event=None):
        if _get(event, "type") == "book_acquired":
            payload = _get(event, "payload") or {}
            title_id = _get(payload, "titleId") or ""
            host = getattr(self.env, "HOST", None)
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
        return None

    async def cliDescribe(self, _params=None):
        return js(CLI)

    async def cliInvoke(self, params=None):
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
