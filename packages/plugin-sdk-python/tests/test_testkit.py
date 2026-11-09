"""`create_test_database(profile=...)`: the served binding behaves like a host grant.

Needs the ``bookclerk-db-testkit`` binary (``BOOKCLERK_DB_TESTKIT`` or on
``PATH``); without it the suite builds one with ``cargo`` when a workspace is
around, and otherwise skips.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import subprocess
import unittest
from pathlib import Path
from typing import Any

from bookclerk_plugin_sdk.testkit import (
    TEST_DATABASE_PROFILES,
    create_test_database,
    default_profile,
    testkit_binary,
)
from bookclerk_plugin_sdk.workerd import PluginError

WORKSPACE = Path(__file__).resolve().parents[3]

MIGRATIONS: list[dict[str, Any]] = [
    {
        "id": "notes-v1",
        "operations": [
            {
                "schema": (
                    "CREATE TABLE IF NOT EXISTS notes "
                    "(id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)"
                )
            }
        ],
    }
]


def _ensure_binary() -> str | None:
    """Find or build the kit binary; ``None`` when neither is possible."""
    if os.environ.get("BOOKCLERK_DB_TESTKIT"):
        return os.environ["BOOKCLERK_DB_TESTKIT"]
    on_path = shutil.which("bookclerk-db-testkit")
    if on_path:
        return on_path
    if not (WORKSPACE / "Cargo.toml").is_file() or shutil.which("cargo") is None:
        return None
    exe = "bookclerk-db-testkit.exe" if os.name == "nt" else "bookclerk-db-testkit"
    built = WORKSPACE / "target" / "debug" / exe
    if not built.is_file():
        subprocess.run(
            ["cargo", "build", "-p", "bookclerk-db-testkit", "--bin", "bookclerk-db-testkit"],
            cwd=WORKSPACE,
            check=True,
        )
    os.environ["BOOKCLERK_DB_TESTKIT"] = str(built)
    return str(built)


BINARY = _ensure_binary()


def _i64(n: int) -> dict[str, Any]:
    """A typed ``int64`` binding value."""
    return {"kind": "int64", "value": n}


def _text(s: str) -> dict[str, Any]:
    """A typed ``text`` binding value."""
    return {"kind": "text", "value": s}


def _run(coro: Any) -> Any:
    """Run ``coro`` on a fresh event loop."""
    return asyncio.run(coro)


class DefaultProfileTests(unittest.TestCase):
    """The profile a test runs under when none is passed."""

    def test_argument_then_environment_then_minimum(self) -> None:
        """An explicit profile wins, then ``bookclerk-plugin test``'s variable, then ``minimum``."""
        saved = os.environ.pop("BOOKCLERK_DB_TESTKIT_PROFILE", None)
        try:
            self.assertEqual(default_profile(None), "minimum")
            os.environ["BOOKCLERK_DB_TESTKIT_PROFILE"] = ""
            self.assertEqual(default_profile(None), "minimum")
            os.environ["BOOKCLERK_DB_TESTKIT_PROFILE"] = "first-party"
            self.assertEqual(default_profile(None), "first-party")
            self.assertEqual(default_profile("d1"), "d1")
            os.environ["BOOKCLERK_DB_TESTKIT_PROFILE"] = "huge"
            with self.assertRaisesRegex(ValueError, "unknown test database profile 'huge'"):
                asyncio.run(create_test_database())
        finally:
            if saved is None:
                os.environ.pop("BOOKCLERK_DB_TESTKIT_PROFILE", None)
            else:
                os.environ["BOOKCLERK_DB_TESTKIT_PROFILE"] = saved


class TestkitBinaryTests(unittest.TestCase):
    """Binary resolution."""

    def test_explicit_path_wins(self) -> None:
        """An explicit path is returned as given."""
        self.assertEqual(testkit_binary("explicit"), "explicit")

    def test_environment_then_path_name(self) -> None:
        """Without a path the environment, then the bare name, is used."""
        saved = os.environ.pop("BOOKCLERK_DB_TESTKIT", None)
        try:
            self.assertEqual(testkit_binary(), "bookclerk-db-testkit")
            os.environ["BOOKCLERK_DB_TESTKIT"] = "/opt/kit"
            self.assertEqual(testkit_binary(), "/opt/kit")
        finally:
            if saved is None:
                os.environ.pop("BOOKCLERK_DB_TESTKIT", None)
            else:
                os.environ["BOOKCLERK_DB_TESTKIT"] = saved

    def test_unknown_profile_is_refused_before_spawning(self) -> None:
        """A profile outside the three is a ``ValueError``."""
        with self.assertRaises(ValueError):
            _run(create_test_database(profile="huge"))  # type: ignore[arg-type]

    def test_missing_binary_is_a_runtime_error(self) -> None:
        """A binary that cannot start is reported, not raised as ``OSError``."""
        with self.assertRaises(RuntimeError):
            _run(create_test_database(binary=str(WORKSPACE / "no-such-bookclerk-db-testkit")))


@unittest.skipIf(BINARY is None, "bookclerk-db-testkit binary unavailable")
class TestDatabaseTests(unittest.TestCase):
    """The served binding under every profile."""

    def test_schema_batch_and_scope_under_every_profile(self) -> None:
        """Schema applied, a batch is one transaction, the scope is the plugin's."""

        async def scenario() -> None:
            for profile in TEST_DATABASE_PROFILES:
                async with await create_test_database(profile=profile, migrations=MIGRATIONS) as db:
                    self.assertEqual(db.profile, profile)
                    limits = await db.limits()
                    self.assertEqual(limits["sqlContractVersion"], 1)
                    insert = db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
                    run = await insert.bind(_i64(1), _text("first")).run()
                    self.assertTrue(run["success"])
                    first = (
                        await db.binding.prepare("SELECT id, body FROM notes WHERE id = ?")
                        .bind(_i64(1))
                        .first()
                    )
                    self.assertEqual(first, {"id": _i64(1), "body": _text("first")})
                    with self.assertRaises(PluginError) as caught:
                        await db.binding.batch(
                            [
                                insert.bind(_i64(2), _text("second")),
                                insert.bind(_i64(1), _text("again")),
                            ]
                        )
                    self.assertNotEqual(caught.exception.code, "invalid_params")
                    rows = (await db.binding.prepare("SELECT id FROM notes ORDER BY id").all())[
                        "results"
                    ]
                    self.assertEqual(rows, [{"id": _i64(1)}])
                    with self.assertRaises(PluginError) as scoped:
                        await db.binding.prepare(
                            "SELECT operation_id FROM bookclerk_receipts"
                        ).all()
                    self.assertEqual(scoped.exception.code, "invalid_params")

        _run(scenario())

    def test_minimum_answers_typed_limits(self) -> None:
        """A result cell past the minimum cap is ``limit_exceeded`` naming the cap."""

        async def scenario() -> None:
            db = await create_test_database(migrations=MIGRATIONS)
            try:
                self.assertEqual(db.profile, "minimum")
                limits = await db.limits()
                cell_cap = int(limits["maxCellBytes"])
                body = "x" * (cell_cap // 2 + 1)
                await (
                    db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
                    .bind(_i64(1), _text(body))
                    .run()
                )
                with self.assertRaises(PluginError) as caught:
                    await (
                        db.binding.prepare("SELECT body || body AS twice FROM notes WHERE id = ?")
                        .bind(_i64(1))
                        .first()
                    )
                err = caught.exception
                self.assertEqual(err.code, "limit_exceeded")
                self.assertIsNotNone(err.details)
                assert err.details is not None
                self.assertEqual(err.details["limit"], "maxCellBytes")
                self.assertEqual(int(err.details["cap"]), cell_cap)
            finally:
                await db.close()
                await db.close()

        _run(scenario())

    def test_first_party_streams_a_large_result(self) -> None:
        """Under first-party, ``all()`` returns a result past the inline cap."""

        async def scenario() -> None:
            async with await create_test_database(
                profile="first-party", migrations=MIGRATIONS
            ) as db:
                limits = await db.limits()
                self.assertGreater(limits["maxStreamedResultBytes"], limits["maxResultBytes"])
                body = "y" * (16 * 1024)
                insert = db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
                for start in range(0, 40, 10):
                    await db.binding.batch(
                        [insert.bind(_i64(i), _text(body)) for i in range(start, start + 10)]
                    )
                rows = (await db.binding.prepare("SELECT id, body FROM notes ORDER BY id").all())[
                    "results"
                ]
                self.assertEqual(len(rows), 40)
                self.assertGreater(len(rows) * len(body), limits["maxResultBytes"])

        _run(scenario())

    def test_a_refused_registration_reports_the_kit_message(self) -> None:
        """A registration that fails the proof surfaces the kit's error."""
        bad = [
            {
                "id": "bad",
                "operations": [
                    {"schema": "CREATE TABLE IF NOT EXISTS bookclerk_receipts (x INTEGER)"}
                ],
            }
        ]
        with self.assertRaises(RuntimeError) as caught:
            _run(create_test_database(migrations=bad))
        self.assertIn("exited before announcing", str(caught.exception))
