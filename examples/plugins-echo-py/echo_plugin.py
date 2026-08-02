#!/usr/bin/env python3
"""Minimal Bookclerk Echo integration guest (experimental Python).

Speaks newline-delimited JSON-RPC 2.0 on stdio (jsonrpc-stdio-v1).
Implements handshake / health / cli.invoke ping for conformance probes.
"""

from __future__ import annotations

import json
import sys
from typing import Any

API_VERSION = 1
PLUGIN_ID = "echo"
KIND = "integration"

CLI_SCHEMA: dict[str, Any] = {
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


def respond(req_id: Any, result: Any) -> None:
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result}) + "\n")
    sys.stdout.flush()


def respond_error(req_id: Any, message: str) -> None:
    sys.stdout.write(
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32000, "message": message},
            }
        )
        + "\n"
    )
    sys.stdout.flush()


def handle(method: str, params: Any) -> Any:
    if method == "handshake":
        return {
            "api_version": API_VERSION,
            "id": PLUGIN_ID,
            "kind": KIND,
            "display_name": "Echo Integration (Python)",
            "capabilities": ["health", "cli"],
            "cli": CLI_SCHEMA,
        }
    if method == "health":
        return {
            "id": PLUGIN_ID,
            "enabled": True,
            "ok": True,
            "detail": "echo-py plugin ready",
        }
    if method == "cli.describe":
        return CLI_SCHEMA
    if method == "cli.invoke":
        params = params or {}
        command = params.get("command")
        if command != "ping":
            raise ValueError(f"unknown command: {command}")
        args = params.get("args") or {}
        message = args.get("message", "hi")
        if not isinstance(message, str):
            message = "hi"
        return {
            "exit_code": 0,
            "stdout": f"pong: {message}\n",
            "stderr": "",
            "json": {"pong": message},
        }
    if method == "shutdown":
        return None
    raise ValueError(f"method not found: {method}")


def main() -> None:
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as err:
            sys.stderr.write(f"invalid request: {err}\n")
            continue
        req_id = req.get("id")
        method = req.get("method", "")
        is_shutdown = method == "shutdown"
        try:
            result = handle(method, req.get("params"))
            respond(req_id, result)
        except Exception as err:  # noqa: BLE001 — map all handler errors to JSON-RPC
            if is_shutdown:
                respond(req_id, None)
            else:
                respond_error(req_id, str(err))
        if is_shutdown:
            break


if __name__ == "__main__":
    main()
