"""Echo workerd guest (Python Workers, api_version = 3).

    from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, js

``bookclerk-workerd`` injects the SDK under that module path. The module-level
``Default`` class is the default entrypoint (``event(batch)`` trigger for the
``book_acquired`` consumer, and ``database_migrations`` for the ``DB``
binding) and ``Cli`` is the ``cli`` entrypoint declared in ``plugin.toml``,
whose ``notes-*`` commands use the database helpers in ``notes.py``.

The module imports under CPython too, where ``tests/`` drive it against the
database test kit.
"""

from __future__ import annotations

import re
from typing import Any

from bookclerk_plugin_sdk.workerd import (
    BookclerkEntrypoint,
    CliEntrypoint,
    EventBatch,
    PluginError,
    QueueBatch,
    cli_args,
    json_payload,
)

from notes import (
    MIGRATIONS,
    NOTES_BINDING,
    add_notes,
    count_notes,
    exceeded_limit,
    list_notes,
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
        },
        {
            "name": "notes-add",
            "about": "Store a note COUNT times in the plugin's own database",
            "args": [
                {
                    "name": "body",
                    "long": "body",
                    "kind": "string",
                    "required": True,
                    "positional": False,
                },
                {
                    "name": "count",
                    "long": "count",
                    "kind": "int",
                    "required": False,
                    "positional": False,
                    "default": "1",
                },
            ],
        },
        {
            "name": "notes-list",
            "about": "List the oldest LIMIT stored notes",
            "args": [
                {
                    "name": "limit",
                    "long": "limit",
                    "kind": "int",
                    "required": False,
                    "positional": False,
                    "default": "100",
                }
            ],
        },
    ]
}

MAX_NOTES_PER_ADD = 10_000
"""Largest ``--count`` ``notes-add`` accepts, so one command stays a modest write."""

MAX_NOTES_PER_LIST = 1_000
"""Largest ``--limit`` ``notes-list`` accepts.

The listing goes back in one CLI reply, so it is bounded even though
``list_notes`` itself streams.
"""

_WHOLE_NUMBER = re.compile(r"[0-9]{1,6}")
"""Plain decimal digits, short enough that the value is always an exact int."""


def whole_number(value: str, maximum: int) -> int | None:
    """Parse a whole-number CLI argument within ``1..=maximum``.

    Args:
        value: The argument as given.
        maximum: Largest value accepted.

    Returns:
        The number, or ``None`` when ``value`` is not plain decimal digits
        in range.
    """
    if _WHOLE_NUMBER.fullmatch(value) is None:
        return None
    n = int(value)
    return n if 1 <= n <= maximum else None


def reply(exit_code: int, stdout: str, stderr: str, payload: Any) -> dict[str, Any]:
    """A CLI reply.

    Args:
        exit_code: Process exit status (``0`` success, ``1`` failure, ``2`` usage).
        stdout: Text for standard output.
        stderr: Text for standard error.
        payload: Structured result for callers that read JSON.

    Returns:
        The ``CliInvokeResult``.
    """
    return {
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "payload": json_payload(payload),
    }


def database_failure(err: Exception, committed: int) -> dict[str, Any]:
    """A CLI reply for a failed database call.

    A typed limit names the cap; any other error is passed on so the host
    reports it.

    Args:
        err: What the binding raised.
        committed: Notes already stored by batches that committed before the
            failure; they stay stored.

    Returns:
        Exit status 1 with the exceeded cap.

    Raises:
        Exception: ``err`` itself when it is not a ``limit_exceeded`` error
            and nothing was stored; otherwise an error that says how many
            notes were: a ``PluginError`` with ``err``'s code and details
            when ``err`` is one, so the host still reports it typed, else a
            ``RuntimeError``.
    """
    limit = exceeded_limit(err)
    kept = f"{committed} notes were stored before the failure" if committed > 0 else ""
    if limit is None:
        if committed == 0:
            raise err
        if isinstance(err, PluginError):
            raise PluginError(err.wire_code, f"{kept}: {err}", err.details) from err
        raise RuntimeError(f"{kept}: {err}") from err
    stderr = (
        f"the database refused the request: {limit.limit} is {limit.cap}, it needed {limit.used}\n"
    )
    if kept:
        stderr += f"{kept}\n"
    return reply(
        1,
        "",
        stderr,
        {
            "limit": {"limit": limit.limit, "cap": limit.cap, "used": limit.used},
            "stored": committed,
        },
    )


class Cli(CliEntrypoint):
    """``cli`` entrypoint: ``bookclerk plugins echo_workerd_python ping --message hi``."""

    async def describe(self):
        """Return the CLI schema."""
        return CLI

    async def invoke(self, params):
        """Handle one command (``CliInvokeParams`` → ``CliInvokeResult``)."""
        command = params.get("command") if isinstance(params, dict) else None
        args = cli_args(params)
        if command == "ping":
            message = args.get("message", "hi")
            return reply(0, f"pong: {message}\n", "", {"pong": message})
        if command == "notes-add":
            return await self._notes_add(args)
        if command == "notes-list":
            return await self._notes_list(args)
        return reply(2, "", f"unknown command {command or ''}\n", None)

    async def _notes_add(self, args: dict[str, str]) -> dict[str, Any]:
        """Store ``--body`` ``--count`` times and report the new total."""
        count = whole_number(args.get("count", "1"), MAX_NOTES_PER_ADD)
        if count is None:
            return reply(2, "", f"--count must be 1 to {MAX_NOTES_PER_ADD}\n", None)
        body = args.get("body", "")
        db = self.env.DB
        committed = 0

        def on_committed(n: int) -> None:
            nonlocal committed
            committed = n

        try:
            stored = await add_notes(db, [body] * count, on_committed)
            total = await count_notes(db)
        except Exception as err:  # noqa: BLE001 - database_failure re-raises what it does not report
            return database_failure(err, committed)
        return reply(
            0, f"stored {stored}; {total} in total\n", "", {"stored": stored, "total": total}
        )

    async def _notes_list(self, args: dict[str, str]) -> dict[str, Any]:
        """Print the oldest ``--limit`` notes, one ``id: body`` line each."""
        limit = whole_number(args.get("limit", "100"), MAX_NOTES_PER_LIST)
        if limit is None:
            return reply(2, "", f"--limit must be 1 to {MAX_NOTES_PER_LIST}\n", None)
        lines: list[str] = []
        notes = list_notes(self.env.DB, limit)
        try:
            async for note in notes:
                lines.append(f"{note.id}: {note.body}")
        except Exception as err:  # noqa: BLE001 - database_failure re-raises what it does not report
            return database_failure(err, 0)
        finally:
            await notes.aclose()
        return reply(0, "".join(f"{line}\n" for line in lines), "", {"count": len(lines)})


class Default(BookclerkEntrypoint):
    """Default entrypoint: event trigger for ``[[events.consumers]]``."""

    async def describe(self):
        """Presentation fields beyond ``plugin.toml``."""
        return {"displayName": "Echo Integration (workerd Python)"}

    def database_migrations(self, binding: str) -> list[dict[str, Any]]:
        """The schema of each ``[[databases]]`` binding, applied before it is granted.

        Args:
            binding: Binding name from ``plugin.toml``.

        Returns:
            The binding's migrations, oldest first.
        """
        return MIGRATIONS if binding == NOTES_BINDING else []

    async def event(self, batch: EventBatch) -> None:
        """Log ``book_acquired`` deliveries and ack every message."""
        for msg in batch.messages:
            if msg.type == "book_acquired":
                title_id = msg.json().get("titleId", "")
                print(f"{PLUGIN_ID} saw book_acquired titleId={title_id}")
            msg.ack()

    async def queue(self, batch: QueueBatch) -> None:
        """Ack every ``echo`` message; a ``text`` body of ``retry`` asks for redelivery."""
        for msg in batch.messages:
            if msg.content_type == "text" and msg.text() == "retry":
                msg.retry(delay_seconds=1, reason="echo retry")
                continue
            print(f"{PLUGIN_ID} saw queue {batch.queue} message {msg.id}")
            # Echo the body onward when the host granted the producer binding.
            echo_out = self.env.get("ECHO_OUT")
            if echo_out is not None:
                await echo_out.send(msg.body, content_type="bytes")
            msg.ack()
