"""Echo workerd guest (Python Workers, api_version = 3).

    from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, js

``bookclerk-workerd`` injects the SDK under that module path. The module-level
``Default`` class is the default entrypoint (``event(batch)`` trigger for the
``book_acquired`` consumer) and ``Cli`` is the ``cli`` entrypoint declared in
``plugin.toml``. This module is workerd-only.
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import (
    BookclerkEntrypoint,
    CliEntrypoint,
    EventBatch,
    cli_args,
    json_payload,
)

PLUGIN_ID = "echo_workerd_python"

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
                    "required": False,
                    "positional": False,
                    "default": "hi",
                }
            ],
        }
    ]
}


class Cli(CliEntrypoint):
    """``cli`` entrypoint: ``bookclerk plugins echo_workerd_python ping --message hi``."""

    async def describe(self):
        """Return the CLI schema."""
        return CLI

    async def invoke(self, params):
        """Handle the ``ping`` command (``CliInvokeParams`` → ``CliInvokeResult``)."""
        command = params.get("command") if isinstance(params, dict) else None
        if command != "ping":
            return {
                "exitCode": 2,
                "stdout": "",
                "stderr": f"unknown command {command or ''}",
                "payload": json_payload(None),
            }
        message = cli_args(params).get("message", "hi")
        return {
            "exitCode": 0,
            "stdout": f"pong: {message}\n",
            "stderr": "",
            "payload": json_payload({"pong": message}),
        }


class Default(BookclerkEntrypoint):
    """Default entrypoint: event trigger for ``[[events.consumers]]``."""

    async def describe(self):
        """Presentation fields beyond ``plugin.toml``."""
        return {"displayName": "Echo Integration (workerd Python)"}

    async def event(self, batch: EventBatch) -> None:
        """Log ``book_acquired`` deliveries and ack every message."""
        for msg in batch.messages:
            if msg.type == "book_acquired":
                title_id = msg.json().get("titleId", "")
                print(f"{PLUGIN_ID} saw book_acquired titleId={title_id}")
            msg.ack()
