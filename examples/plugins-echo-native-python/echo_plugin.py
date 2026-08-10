#!/usr/bin/env python3
"""Reference Echo integration — native Python (PyInstaller).

Subclasses BookclerkPlugin; BookclerkPluginGuest.serve is the stdio runner.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent


def _ensure_sdk_path() -> None:
    candidates: list[Path] = []
    env = os.environ.get("BOOKCLERK_PLUGIN_SDK_PYTHON")
    if env:
        candidates.append(Path(env))
    candidates.append(_HERE / "sdk")
    candidates.append(_HERE.parents[1] / "packages/plugin-sdk-python/src")
    for cand in candidates:
        if cand.is_dir() and str(cand) not in sys.path:
            sys.path.insert(0, str(cand))
            return


_ensure_sdk_path()

from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest  # noqa: E402

API_VERSION = 1
PLUGIN_ID = "echo-native-python"
KIND = "integration"

CLI_SCHEMA = {
    "commands": [
        {
            "name": "ping",
            "about": "Probe echo plugin",
            "args": [
                {
                    "name": "message",
                    "long": "message",
                    "short": "m",
                    "kind": "string",
                    "required": False,
                    "default": "hi",
                    "about": "Message to echo",
                    "positional": False,
                }
            ],
        }
    ]
}


class EchoPlugin(BookclerkPlugin):
    def handshake(self, _params):
        return {
            "apiVersion": API_VERSION,
            "id": PLUGIN_ID,
            "kind": KIND,
            "displayName": "Echo Integration (native Python)",
            "capabilities": ["health", "diagnose", "onEvent", "cli"],
            "cli": CLI_SCHEMA,
        }

    def health(self):
        return {
            "id": PLUGIN_ID,
            "enabled": True,
            "ok": True,
            "detail": "echo-native-python ready",
        }

    def diagnose(self):
        return {"lines": ["echo-native-python diagnose: ok"]}

    def on_event(self, _event):
        return None

    def cli_describe(self):
        return CLI_SCHEMA

    def cli_invoke(self, params):
        command = (params or {}).get("command")
        if command != "ping":
            err = RuntimeError(f"unknown command: {command}")
            err.code = "invalid_params"  # type: ignore[attr-defined]
            raise err
        args = (params or {}).get("args") or {}
        message = args.get("message") if isinstance(args.get("message"), str) else "hi"
        return {"exitCode": 0, "stdout": f"pong: {message}\n", "stderr": ""}


if __name__ == "__main__":
    BookclerkPluginGuest.serve(EchoPlugin())
