"""The entrypoint glue: CLI commands and the migrations hook.

``env.DB`` is the binding ``bookclerk-db-testkit`` serves under the profile
``bookclerk-plugin test`` names, granted the way the host grants it, through
the invocation context ``bookclerkInvoke`` reads.
"""

from __future__ import annotations

import unittest
from typing import Any

from bookclerk_plugin_sdk import testkit
from bookclerk_plugin_sdk.limits import LimitExceeded
from bookclerk_plugin_sdk.workerd import PluginError

from notes import MIGRATIONS
from plugin import Cli, Default


class _FailingBatches:
    """The binding, except that ``batch()`` call ``fail_on`` raises ``failure``."""

    def __init__(self, binding: Any, failure: BaseException, fail_on: int) -> None:
        """Wrap ``binding``.

        Args:
            binding: The served binding every other call reaches.
            failure: What the failing ``batch()`` raises.
            fail_on: Which ``batch()`` call fails, counted from 1.
        """
        self._binding = binding
        self._failure = failure
        self._fail_on = fail_on
        self._batches = 0

    def __getattr__(self, name: str) -> Any:
        """Every other call goes to the binding."""
        return getattr(self._binding, name)

    async def batch(self, statements: Any) -> Any:
        """Commit through the binding, except the failing call."""
        self._batches += 1
        if self._batches == self._fail_on:
            raise self._failure
        return await self._binding.batch(statements)


async def run(binding: Any, command: str, **args: str) -> dict[str, Any]:
    """Invoke one CLI command with ``binding`` granted as ``DB``.

    Args:
        binding: The binding the command sees as ``env.DB``.
        command: Command name.
        **args: Arguments by name.

    Returns:
        The ``CliInvokeResult``.
    """
    params = {
        "command": command,
        "args": [{"name": name, "value": value} for name, value in args.items()],
    }
    context = {"databases": [{"name": "DB", "database": binding}]}
    return await Cli().bookclerkInvoke(context, "invoke", params)


class CliTests(unittest.IsolatedAsyncioTestCase):
    """The ``cli`` entrypoint over a served binding."""

    db: testkit.TestDatabase

    async def asyncSetUp(self) -> None:
        """Serve an empty notes table under the profile the runner names."""
        self.db = await testkit.create_test_database(migrations=MIGRATIONS)

    async def asyncTearDown(self) -> None:
        """Stop the kit."""
        await self.db.close()

    async def test_the_cli_schema_lists_every_command(self) -> None:
        schema = await Cli().describe()
        self.assertEqual(
            [c["name"] for c in schema["commands"]], ["ping", "notes-add", "notes-list"]
        )

    async def test_ping_echoes_its_message(self) -> None:
        r = await run(self.db.binding, "ping", message="hello")
        self.assertEqual(r["exitCode"], 0)
        self.assertEqual(r["stdout"], "pong: hello\n")

    async def test_notes_add_stores_the_note_count_times_and_notes_list_prints_them(self) -> None:
        added = await run(self.db.binding, "notes-add", body="remember", count="3")
        self.assertEqual(added["exitCode"], 0, added["stderr"])
        self.assertEqual(added["stdout"], "stored 3; 3 in total\n")
        listed = await run(self.db.binding, "notes-list")
        self.assertEqual(listed["exitCode"], 0, listed["stderr"])
        lines = listed["stdout"].rstrip("\n").split("\n")
        self.assertEqual(len(lines), 3)
        for line in lines:
            self.assertRegex(line, r"^\d+: remember$")

    async def test_notes_add_refuses_a_count_out_of_range(self) -> None:
        for count in ("0", "-1", "1.5", "lots", "10001", "0x10", "1e3", " 5 ", "", "５"):
            r = await run(self.db.binding, "notes-add", body="x", count=count)
            self.assertEqual(r["exitCode"], 2, count)
            self.assertIn("--count must be 1 to 10000", r["stderr"])

    async def test_notes_list_stops_at_limit_and_refuses_a_limit_out_of_range(self) -> None:
        await run(self.db.binding, "notes-add", body="many", count="5")
        r = await run(self.db.binding, "notes-list", limit="2")
        self.assertEqual(r["exitCode"], 0, r["stderr"])
        self.assertEqual(len(r["stdout"].rstrip("\n").split("\n")), 2)
        for limit in ("0", "1001", "two"):
            bad = await run(self.db.binding, "notes-list", limit=limit)
            self.assertEqual(bad["exitCode"], 2, limit)
            self.assertIn("--limit must be 1 to 1000", bad["stderr"])

    async def test_notes_add_says_how_many_notes_a_failed_write_had_already_stored(self) -> None:
        caps = await self.db.limits()
        count = str(caps["maxStatements"] * 2)
        limit = PluginError.limit_exceeded(LimitExceeded(limit="maxRequestBytes", cap=1, used=2))
        r = await run(
            _FailingBatches(self.db.binding, limit, 2), "notes-add", body="b", count=count
        )
        self.assertEqual(r["exitCode"], 1)
        self.assertRegex(
            r["stderr"],
            r"maxRequestBytes is 1, it needed 2\n\d+ notes were stored before the failure\n$",
        )
        broken = PluginError.from_wire("unavailable", "unavailable")
        with self.assertRaisesRegex(
            RuntimeError, r"^\d+ notes were stored before the failure: unavailable$"
        ) as wrapped:
            await run(
                _FailingBatches(self.db.binding, broken, 2), "notes-add", body="b", count=count
            )
        self.assertIs(wrapped.exception.__cause__, broken)
        with self.assertRaises(PluginError) as plain:
            await run(_FailingBatches(self.db.binding, broken, 1), "notes-add", body="b")
        self.assertIs(plain.exception, broken)

    async def test_notes_add_reports_the_cap_a_note_is_too_large_for(self) -> None:
        caps = await self.db.limits()
        r = await run(
            self.db.binding,
            "notes-add",
            body="x" * (min(caps["maxCellBytes"], caps["maxPayloadBytes"]) + 1),
        )
        self.assertEqual(r["exitCode"], 1)
        self.assertRegex(
            r["stderr"], r"^the database refused the request: max\w+ is \d+, it needed \d+\n$"
        )

    async def test_an_unknown_command_is_a_usage_error(self) -> None:
        r = await run(self.db.binding, "nope")
        self.assertEqual(r["exitCode"], 2)
        self.assertEqual(r["stderr"], "unknown command nope\n")


class MigrationsHookTests(unittest.IsolatedAsyncioTestCase):
    """The default entrypoint's ``database_migrations``."""

    async def test_the_migrations_hook_answers_for_the_db_binding_only(self) -> None:
        plugin = Default()
        self.assertEqual(await plugin.bookclerkDatabaseMigrations("DB"), MIGRATIONS)
        self.assertEqual(await plugin.bookclerkDatabaseMigrations("OTHER"), [])


if __name__ == "__main__":
    unittest.main()
