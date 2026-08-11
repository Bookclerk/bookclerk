"""Workerd Python Workers template — package import for BookclerkPlugin.

    from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

plugin.toml:
  runtime = "workerd"
  [workerd]
  main_module = "plugin.py"
  compatibility_flags = ["python_workers", "disable_python_external_sdk"]

`bookclerk-workerd` injects the SDK. Native guests:

    from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

API_VERSION = 1
PLUGIN_ID = "my_python_plugin"
KIND = "integration"


class Default(BookclerkPlugin):
    async def handshake(self, _params=None):
        return js(
            {
                "apiVersion": API_VERSION,
                "id": PLUGIN_ID,
                "kind": KIND,
                "displayName": "My Python Plugin",
                "capabilities": ["health", "diagnose", "cli"],
            }
        )

    async def health(self, _params=None):
        return js({"ok": True, "id": PLUGIN_ID, "detail": "python workerd plugin ready"})

    async def diagnose(self, _params=None):
        return js({"lines": [f"{PLUGIN_ID}: ok"]})

    async def cliInvoke(self, params=None):
        command = params.get("command") if hasattr(params, "get") else None
        if command != "ping":
            return js({"exitCode": 2, "stderr": f"unknown command {command}"})
        args = (params.get("args") if hasattr(params, "get") else {}) or {}
        message = args.get("message") if hasattr(args, "get") else "hi"
        if not isinstance(message, str):
            message = "hi"
        return js({"exitCode": 0, "stdout": f"pong: {message}\n"})
