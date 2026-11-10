"""The notes helpers against the binding a host grants.

The binding is served by ``bookclerk-db-testkit`` under one capability
profile. Run through ``bookclerk-plugin test .`` (``cargo plugin -- test
examples/plugins-echo-workerd-python`` in the workspace), which runs this
suite once per profile and names the kit binary and the profile in the
environment. Every assertion is sized from the profile's own limits, so the
same tests prove the code on the shipped adapters, on D1 and on the smallest
advertisement a host accepts.
"""

from __future__ import annotations

import math
import unittest
from typing import Any

from bookclerk_plugin_sdk import testkit
from bookclerk_plugin_sdk.limits import LimitExceeded
from bookclerk_plugin_sdk.workerd import PluginError

from notes import (
    MIGRATIONS,
    Note,
    add_notes,
    count_notes,
    exceeded_limit,
    list_notes,
    note_from_row,
    note_too_large,
)


def _int64(value: int) -> dict[str, Any]:
    """An INTEGER cell as the binding returns it."""
    return {"kind": "int64", "value": value}


def _text(value: str) -> dict[str, Any]:
    """A TEXT cell as the binding returns it."""
    return {"kind": "text", "value": value}


class _ScriptedStatement:
    """A prepared statement whose ``all()`` answers from a script."""

    def __init__(self, answer: Any) -> None:
        """Keep the script.

        Args:
            answer: Called with the bound values; returns the page or raises.
        """
        self._answer = answer
        self._values: tuple[Any, ...] = ()

    def bind(self, *values: Any) -> _ScriptedStatement:
        """Remember the bound values."""
        self._values = values
        return self

    async def all(self) -> dict[str, Any]:
        """Answer from the script."""
        return self._answer(*self._values)


class _ScriptedBinding:
    """A session that does not stream and allows ``max_rows`` rows a page."""

    def __init__(self, answer: Any, max_rows: int = 4) -> None:
        """Keep the script.

        Args:
            answer: Called with each read's bound values.
            max_rows: The advertised ``maxResultRows``.
        """
        self._answer = answer
        self._max_rows = max_rows

    async def limits(self) -> dict[str, int]:
        """The advertised caps the helper reads."""
        return {"maxStreamedResultBytes": 0, "maxResultRows": self._max_rows}

    def prepare(self, _sql: str) -> _ScriptedStatement:
        """A statement answered from the script."""
        return _ScriptedStatement(self._answer)


TOO_WIDE = PluginError.limit_exceeded(LimitExceeded(limit="maxResultBytes", cap=10, used=20))
"""The refusal a page whose bytes pass the inline cap gets."""


class NotesTests(unittest.IsolatedAsyncioTestCase):
    """The helpers on a fresh binding per test."""

    db: testkit.TestDatabase

    async def asyncSetUp(self) -> None:
        """Serve an empty notes table under the profile the runner names."""
        self.db = await testkit.create_test_database(migrations=MIGRATIONS)

    async def asyncTearDown(self) -> None:
        """Stop the kit."""
        await self.db.close()

    async def all_notes(self) -> list[Note]:
        """Drain :func:`list_notes`."""
        return [note async for note in list_notes(self.db.binding)]

    async def note_budget(self) -> int:
        """The most bytes one note may hold under the profile.

        A statement's SQL and binds share ``maxPayloadBytes`` (3,667 bytes
        on D1), and a reply cell is held to ``maxCellBytes``.
        """
        caps = await self.db.limits()
        return min(caps["maxCellBytes"], caps["maxPayloadBytes"])

    async def bodies_past_the_inline_cap(self) -> list[str]:
        """Note bodies that together pass the profile's inline result cap.

        Neither a single inline read nor a single request holds them all.
        Each is half the per-note budget, in multibyte text so byte and
        character counts differ ("né" is 3 bytes).
        """
        caps = await self.db.limits()
        body = "né" * ((await self.note_budget()) // 2 // 3)
        count = math.ceil(2 * caps["maxResultBytes"] / len(body.encode("utf-8"))) + 1
        return [f"{i} {body}" for i in range(count)]

    async def test_the_migrations_create_the_notes_table(self) -> None:
        self.assertEqual(await count_notes(self.db.binding), 0)

    async def test_add_notes_splits_a_long_write_into_requests_that_fit(self) -> None:
        bodies = await self.bodies_past_the_inline_cap()
        self.assertEqual(await add_notes(self.db.binding, bodies), len(bodies))
        self.assertEqual(await count_notes(self.db.binding), len(bodies))

    async def test_list_notes_returns_every_note_in_order_past_the_inline_cap(self) -> None:
        bodies = await self.bodies_past_the_inline_cap()
        await add_notes(self.db.binding, bodies)
        notes = await self.all_notes()
        self.assertEqual([n.body for n in notes], bodies)
        self.assertEqual([n.id for n in notes], sorted({n.id for n in notes}))
        # The plain inline read the helper avoids is refused with the typed
        # error whenever the session cannot stream it.
        caps = await self.db.limits()
        if caps["maxStreamedResultBytes"] == 0:
            with self.assertRaises(PluginError) as refused:
                await self.db.binding.prepare("SELECT id, body FROM notes").all()
            self.assertIsNotNone(exceeded_limit(refused.exception))

    async def test_list_notes_yields_nothing_for_an_empty_table(self) -> None:
        self.assertEqual(await self.all_notes(), [])

    async def test_list_notes_stops_reading_when_the_caller_stops(self) -> None:
        await add_notes(self.db.binding, ["a", "b", "c"])
        notes = list_notes(self.db.binding)
        first = await anext(notes)
        await notes.aclose()
        self.assertEqual(first.body, "a")
        self.assertEqual(await count_notes(self.db.binding), 3)

    async def test_a_note_past_the_per_note_budget_is_a_typed_limit_and_nothing_is_stored(
        self,
    ) -> None:
        with self.assertRaises(PluginError) as refused:
            await add_notes(self.db.binding, ["x" * ((await self.note_budget()) + 1)])
        limit = exceeded_limit(refused.exception)
        self.assertIsNotNone(limit, f"expected limit_exceeded, got {refused.exception}")
        assert limit is not None
        self.assertIn(limit.limit, ("maxCellBytes", "maxPayloadBytes", "maxRequestBytes"))
        self.assertGreater(limit.used, limit.cap)
        self.assertEqual(await count_notes(self.db.binding), 0)

    async def test_add_notes_reports_each_committed_batch(self) -> None:
        caps = await self.db.limits()
        seen: list[int] = []
        count = caps["maxStatements"] * 2 + 1
        bodies = [f"n{i}" for i in range(count)]
        self.assertEqual(await add_notes(self.db.binding, bodies, seen.append), count)
        self.assertGreaterEqual(len(seen), 3, f"batches {seen}")
        self.assertEqual(seen[-1], count)
        self.assertEqual(seen, sorted(set(seen)))


class PagingTests(unittest.IsolatedAsyncioTestCase):
    """The key-paged read, on scripted bindings no kit profile matches."""

    async def test_list_notes_halves_a_wide_page_then_grows_back(self) -> None:
        # Four rows a page with no streamed reads, where note 2 is so wide
        # that it fits a page only alone.
        stored = [{"id": _int64(i), "body": _text(f"note {i}")} for i in range(1, 6)]
        page_sizes: list[int] = []

        def answer(after: dict[str, Any], size: dict[str, Any]) -> dict[str, Any]:
            page_sizes.append(size["value"])
            rows = [r for r in stored if r["id"]["value"] > after["value"]][: size["value"]]
            if len(rows) > 1 and any(r["id"]["value"] == 2 for r in rows):
                raise TOO_WIDE
            return {"results": rows}

        notes = [note async for note in list_notes(_ScriptedBinding(answer))]
        self.assertEqual([n.id for n in notes], [1, 2, 3, 4, 5])
        # Halved to fit note 2, then doubled back to the row cap after it.
        self.assertEqual(page_sizes, [4, 2, 1, 2, 1, 2, 4])

    async def test_one_row_past_the_cap_or_any_other_error_is_not_retried(self) -> None:
        def refuse(*_values: Any) -> dict[str, Any]:
            raise TOO_WIDE

        with self.assertRaises(PluginError) as refused:
            await anext(list_notes(_ScriptedBinding(refuse, max_rows=1)))
        self.assertIs(refused.exception, TOO_WIDE)

        broken = PluginError.from_wire("unavailable", "unavailable")

        def fail(*_values: Any) -> dict[str, Any]:
            raise broken

        with self.assertRaises(PluginError) as failed:
            await anext(list_notes(_ScriptedBinding(fail)))
        self.assertIs(failed.exception, broken)


class ShapeTests(unittest.IsolatedAsyncioTestCase):
    """Typed errors and row shapes, without a database."""

    def test_exceeded_limit_reads_only_typed_limit_errors(self) -> None:
        limit = LimitExceeded(limit="maxResultBytes", cap=10, used=11)
        self.assertEqual(exceeded_limit(PluginError.limit_exceeded(limit)), limit)
        self.assertIsNone(
            exceeded_limit(RuntimeError("limit exceeded: maxResultBytes is 10; used 11"))
        )
        self.assertIsNone(exceeded_limit(PluginError.from_wire("unavailable", "down")))

    def test_note_too_large_reads_as_a_typed_limit(self) -> None:
        err = note_too_large(5, 4)
        self.assertEqual(exceeded_limit(err), LimitExceeded(limit="maxCellBytes", cap=4, used=5))
        self.assertEqual(str(err), "limit exceeded: maxCellBytes is 4; used 5")

    async def test_count_notes_refuses_a_count_of_the_wrong_shape(self) -> None:
        class Scripted:
            def __init__(self, row: Any) -> None:
                self._row = row

            def prepare(self, _sql: str) -> Scripted:
                return self

            async def first(self) -> Any:
                return self._row

        self.assertEqual(await count_notes(Scripted({"n": _int64(3)})), 3)
        with self.assertRaisesRegex(ValueError, "unexpected shape"):
            await count_notes(Scripted(None))
        with self.assertRaisesRegex(ValueError, "unexpected shape"):
            await count_notes(Scripted({"n": _text("3")}))

    def test_note_from_row_refuses_a_row_of_the_wrong_shape(self) -> None:
        self.assertEqual(
            note_from_row({"id": _int64(7), "body": _text("hi")}), Note(id=7, body="hi")
        )
        with self.assertRaisesRegex(ValueError, "unexpected shape"):
            note_from_row({"id": _text("7")})


if __name__ == "__main__":
    unittest.main()
