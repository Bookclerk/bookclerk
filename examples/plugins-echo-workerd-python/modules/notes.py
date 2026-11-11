"""A small ``notes`` table on the plugin's own ``[[databases]]`` binding.

Written the way plugin database code should be: sized from ``limits()``,
split with ``chunk()``, read with ``iterate()`` when the session streams,
and paged by key when it does not. Typed ``limit_exceeded`` errors are
handled by the cap they name (``PluginError.limit``), never by matching the
message.

The module runs unchanged inside the isolate and under CPython, where
``tests/test_notes.py`` drives it through ``create_test_database()`` under
every capability profile.

See docs/plugins/database-limits.md.
"""

from __future__ import annotations

from collections.abc import AsyncGenerator, Callable, Sequence
from contextlib import aclosing
from dataclasses import dataclass
from typing import Any, cast

from bookclerk_plugin_sdk.db_value import streams_results
from bookclerk_plugin_sdk.limits import LimitExceeded
from bookclerk_plugin_sdk.workerd import PluginError

NOTES_BINDING = "DB"
"""The ``[[databases]]`` binding name in ``plugin.toml`` that holds the notes."""

MIGRATIONS: list[dict[str, Any]] = [
    {
        "id": "notes-v1",
        "operations": [
            {
                "schema": "CREATE TABLE IF NOT EXISTS notes "
                "(id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)"
            }
        ],
    }
]
"""The ``database_migrations`` registration for :data:`NOTES_BINDING`.

Applied once, in order, before the binding is handed to the plugin.
"""


@dataclass(frozen=True)
class Note:
    """One stored note.

    Attributes:
        id: Identity the database assigned, increasing in insertion order.
        body: The note text as written.
    """

    id: int
    body: str


def exceeded_limit(err: BaseException) -> LimitExceeded | None:
    """The cap a database error exceeded, when it is a typed ``limit_exceeded``.

    Args:
        err: Anything a binding call raised.

    Returns:
        The exceeded cap, or ``None`` for any other error.
    """
    return err.limit if isinstance(err, PluginError) else None


def note_too_large(used: int, cap: int) -> PluginError:
    """A note refused before it is sent: storable, but never returnable.

    A reply cell is held to ``maxCellBytes``, so a longer note could be
    written under a larger ``maxPayloadBytes`` and then never read back.

    Args:
        used: The note's size in UTF-8 bytes.
        cap: The session's ``maxCellBytes``.

    Returns:
        A typed ``limit_exceeded`` error, read with :func:`exceeded_limit`
        like the binding's own.
    """
    return PluginError.limit_exceeded(LimitExceeded(limit="maxCellBytes", cap=cap, used=used))


def _text(value: str) -> dict[str, Any]:
    """A TEXT bind value."""
    return {"kind": "text", "value": value}


def _int64(value: int) -> dict[str, Any]:
    """An INTEGER bind value."""
    return {"kind": "int64", "value": value}


def note_from_row(row: dict[str, Any]) -> Note:
    """Read one stored row back into a :class:`Note`.

    Args:
        row: Column name to typed value, as the binding returns rows.

    Returns:
        The note.

    Raises:
        ValueError: When a column is missing or has an unexpected type.
    """
    note_id = row.get("id")
    body = row.get("body")
    if (
        not isinstance(note_id, dict)
        or note_id.get("kind") != "int64"
        or not isinstance(body, dict)
        or body.get("kind") != "text"
    ):
        raise ValueError("notes row has an unexpected shape")
    return Note(id=int(note_id["value"]), body=str(body["value"]))


async def add_notes(
    db: Any,
    bodies: Sequence[str],
    on_committed: Callable[[int], None] | None = None,
) -> int:
    """Store ``bodies`` as new notes.

    Every body is first checked against the session's ``maxCellBytes``: a
    note the database would accept but could never return is refused up
    front. Then ``chunk()`` splits the inserts into batches that each fit
    one request under the session's ``maxStatements`` and
    ``maxRequestBytes``. Each batch commits atomically; the loop does not,
    so when a later batch fails the notes of the earlier ones stay stored.
    ``on_committed`` hears the running total after each batch, so a caller
    can say how far a failed write got.

    Args:
        db: The plugin's notes binding (``env.DB``).
        bodies: Note texts, stored in order.
        on_committed: Called with the number of notes stored so far after
            each batch commits.

    Returns:
        How many notes were stored.

    Raises:
        PluginError: ``limit_exceeded`` before anything is written when a
            body passes ``maxCellBytes``; from the binding, ``limit_exceeded``
            included when one statement passes ``maxPayloadBytes`` or
            ``maxRequestBytes``.
    """
    caps = await db.limits()
    for body in bodies:
        used = len(body.encode("utf-8"))
        if used > caps["maxCellBytes"]:
            raise note_too_large(used, caps["maxCellBytes"])
    inserts = [db.prepare("INSERT INTO notes (body) VALUES (?)").bind(_text(b)) for b in bodies]
    stored = 0
    async for batch in db.chunk(inserts):
        await db.batch(batch)
        stored += len(batch)
        if on_committed is not None:
            on_committed(stored)
    return stored


async def count_notes(db: Any) -> int:
    """Count the stored notes.

    Args:
        db: The plugin's notes binding.

    Returns:
        The number of notes.

    Raises:
        ValueError: When the count does not come back as one INTEGER.
    """
    row = await db.prepare("SELECT count(*) AS n FROM notes").first()
    n = row.get("n") if isinstance(row, dict) else None
    if not isinstance(n, dict) or n.get("kind") != "int64":
        raise ValueError("notes count has an unexpected shape")
    return int(n["value"])


async def list_notes(db: Any, limit: int | None = None) -> AsyncGenerator[Note, None]:
    """Yield the notes in id order without holding them all in memory.

    When the session streams (``streams_results(limits())``), one
    ``iterate()`` reads the whole table as a single snapshot, a frame at a
    time. When it does not, every read is held to the inline caps, so the
    notes are read in pages keyed on ``id`` (never ``OFFSET``), each sized
    from ``maxResultRows``, halved whenever a page's bytes pass
    ``maxResultBytes``, and doubled again (up to ``maxResultRows``) after
    each full page, so one run of wide notes does not slow the rest of the
    walk. With ``limit``, no page asks for more rows than are still
    wanted.

    Stopping early (``break``, or ``aclose()`` on the generator) ends the read.

    Args:
        db: The plugin's notes binding.
        limit: The most notes to yield, or ``None`` for all of them.

    Yields:
        The notes, oldest first, at most ``limit`` of them.

    Raises:
        PluginError: From the binding: ``limit_exceeded`` when even the
            streamed budget is too small, or one note alone passes the
            inline caps.
    """
    left = limit
    if left is not None and left <= 0:
        return
    caps = await db.limits()
    if streams_results(caps):
        # Closing the read when the caller stops early ends the stream.
        streamed = cast(
            AsyncGenerator[dict[str, Any], None],
            db.prepare("SELECT id, body FROM notes ORDER BY id").iterate(),
        )
        async with aclosing(streamed) as rows:
            async for row in rows:
                yield note_from_row(row)
                if left is not None:
                    left -= 1
                    if left == 0:
                        return
        return
    max_page = max(1, caps["maxResultRows"])
    page_size = max_page
    after = 0
    while True:
        if left is not None:
            page_size = min(page_size, left)
        try:
            page = (
                await db.prepare("SELECT id, body FROM notes WHERE id > ? ORDER BY id LIMIT ?")
                .bind(_int64(after), _int64(page_size))
                .all()
            )
        except PluginError as err:
            limit = exceeded_limit(err)
            if limit is not None and limit.limit == "maxResultBytes" and page_size > 1:
                page_size = max(1, page_size // 2)
                continue
            raise
        rows = page.get("results") or []
        for row in rows:
            note = note_from_row(row)
            after = note.id
            yield note
        if left is not None:
            left -= len(rows)
            if left <= 0:
                return
        if len(rows) < page_size:
            return
        page_size = min(max_page, page_size * 2)
