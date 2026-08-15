"""Echo workerd guest (id `echo_native_python`, api_version = 2).

This example now validates workerd hosting, not a Python Cap'n Proto stack.
Pattern matches `plugins-echo-workerd-python`.
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import BookclerkPlugin, Integration, js

API_VERSION = 2
PLUGIN_ID = "echo_native_python"
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
                "detail": "echo_native_python ready",
            }
        )

    async def diagnose(self, _params=None):
        return js({"lines": ["echo_native_python diagnose: ok"]})

    async def onEvent(self, _event=None):
        return js({"kind": "ack"})


class Default(BookclerkPlugin):
    """Bookclerk plugin entrypoint (workerd `entrypoint = \"default\"`)."""

    async def describe(self):
        return js(
            {
                "apiVersion": API_VERSION,
                "id": PLUGIN_ID,
                "kind": KIND,
                "displayName": "Echo Integration (native Python)",
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
