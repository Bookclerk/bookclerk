"""Workerd Python Workers template — ``BookclerkEntrypoint`` author model.

Example main module for ``runtime = "workerd"`` guests (``api_version = 3``).
Authors copy and adapt this file as ``modules/plugin.py``:

    from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, js

``plugin.toml`` should declare:

    runtime = "workerd"
    entrypoints = ["cli"]
    [workerd]
    main_module = "plugin.py"
    compatibility_flags = ["python_workers", "disable_python_external_sdk"]
    [[events.consumers]]
    type = "book_acquired"

``bookclerk-workerd`` injects the SDK. The module-level ``Default`` class is the
default entrypoint (event / job triggers); each name in ``entrypoints`` maps to
a module-level class of the same capitalized name (``Cli``, ``Storage``, …).
Native guests use Rust ``serve`` / ``PluginWorker``.
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import (
    BookclerkEntrypoint,
    CliEntrypoint,
    EventBatch,
    cli_args,
    json_payload,
)

PLUGIN_ID = "my_python_plugin"
"""Example plugin id; replace before shipping (must match ``plugin.toml``)."""

CLI = {
    "commands": [
        {
            "name": "ping",
            "about": "Probe the plugin",
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
"""``CliSchema`` advertised by the ``cli`` entrypoint."""


class Cli(CliEntrypoint):
    """``cli`` entrypoint: ``bookclerk plugins my_python_plugin ping --message hi``."""

    async def describe(self):
        """Return the CLI schema.

        Returns:
            The ``CliSchema`` dict.
        """
        return CLI

    async def invoke(self, params):
        """Handle the sample ``ping`` command.

        Args:
            params: ``CliInvokeParams`` (``command`` plus ``args`` name/value pairs).

        Returns:
            ``CliInvokeResult`` with ``exitCode``, ``stdout``, ``stderr``, ``payload``.
        """
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
    """Default entrypoint: ``event(batch)`` for ``[[events.consumers]]``."""

    async def describe(self):
        """Presentation fields beyond ``plugin.toml`` (identity comes from the manifest).

        Returns:
            Partial ``PluginDescribe`` dict.
        """
        return {"displayName": "My Python Plugin"}

    async def event(self, batch: EventBatch) -> None:
        """Handle one delivered batch; untouched messages are acked on return.

        Args:
            batch: Delivered events (``batch.messages``).
        """
        for msg in batch.messages:
            if msg.type == "book_acquired":
                print(f"{PLUGIN_ID} saw book_acquired titleId={msg.json().get('titleId', '')}")
            msg.ack()
