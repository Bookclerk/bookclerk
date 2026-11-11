"""Tests for the ``scan_page`` storefront paging helper."""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from bookclerk_plugin_sdk import StorePage, scan_page
from bookclerk_plugin_sdk._abi import MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS
from bookclerk_plugin_sdk.workerd import PluginError

Fetch = Callable[[str, Any], Awaitable[StorePage]]


def _book(i: int) -> Any:
    """A title with only the field these tests read.

    Args:
        i: Title number.

    Returns:
        The title.
    """
    return {"productId": f"p{i}"}


def _params(cursor: str = "", sync_token: str = "") -> Any:
    """Scan params for one call.

    Args:
        cursor: The call's cursor.
        sync_token: The pass's sync token.

    Returns:
        The params.
    """
    return {"accountId": "a1", "cursor": cursor, "syncToken": sync_token}


def _run_pass(fetch: Fetch, sync_token: str = "") -> list[Any]:
    """Runs a whole pass the way the host does.

    Args:
        fetch: The store fetch.
        sync_token: The pass's sync token.

    Returns:
        Every reply of the pass.
    """
    replies: list[Any] = []
    cursor = ""
    for _ in range(1000):
        reply = asyncio.run(scan_page(_params(cursor, sync_token), fetch))
        assert len(reply["books"]) <= MAX_SCAN_PAGE_BOOKS
        assert len(reply["removed"]) <= MAX_SCAN_PAGE_BOOKS
        replies.append(reply)
        if reply["nextCursor"] == "":
            return replies
        assert reply["nextCursor"] != cursor
        cursor = reply["nextCursor"]
    raise AssertionError("the pass did not end")


def test_large_store_pages_are_sliced_in_order() -> None:
    """Every title arrives once, in order; only the last reply ends the pass."""
    sizes = [MAX_SCAN_PAGE_BOOKS * 2 + 7, 4, MAX_SCAN_PAGE_BOOKS]
    starts = [sum(sizes[:i]) for i in range(len(sizes))]
    fetches: list[str] = []

    async def fetch(store_cursor: str, params: Any) -> StorePage:
        fetches.append(store_cursor)
        assert params["accountId"] == "a1"
        index = 0 if store_cursor == "" else int(store_cursor) - 1
        books = [_book(starts[index] + i) for i in range(sizes[index])]
        if index == len(sizes) - 1:
            return {"books": books, "syncToken": "t2", "complete": True}
        return {"books": books, "next": str(index + 2)}

    replies = _run_pass(fetch)
    ids = [b["productId"] for r in replies for b in r["books"]]
    assert ids == [f"p{i}" for i in range(sum(sizes))]
    assert len(replies) == 5
    assert all(r["syncToken"] == "" and not r["complete"] for r in replies[:-1])
    assert replies[-1]["syncToken"] == "t2"
    assert replies[-1]["complete"] is True
    assert fetches == ["", "", "", "2", "3"]


def test_one_store_page_is_one_reply() -> None:
    """A store that does not page answers in one reply."""

    async def fetch(_cursor: str, _params: Any) -> StorePage:
        return {"books": [_book(0)], "syncToken": "t1", "complete": True}

    assert _run_pass(fetch) == [
        {
            "books": [_book(0)],
            "removed": [],
            "nextCursor": "",
            "syncToken": "t1",
            "complete": True,
            "storePages": 1,
        }
    ]


def test_removed_ids_are_sliced_with_the_titles() -> None:
    """A delta pass's removed ids are split across replies too."""
    removed = [f"r{i}" for i in range(MAX_SCAN_PAGE_BOOKS + 1)]

    async def fetch(_cursor: str, _params: Any) -> StorePage:
        return {"books": [_book(0)], "removed": removed, "syncToken": "t3", "storePages": 2}

    replies = _run_pass(fetch, "t2")
    assert [x for r in replies for x in r["removed"]] == removed
    assert [r["storePages"] for r in replies] == [2, 2]
    assert replies[-1]["syncToken"] == "t3"
    assert replies[-1]["complete"] is False


@pytest.mark.parametrize(
    "cursor", ["3", "sp1.", "sp1.x.1", "sp1.01.1", "sp1.7.1", "sp2.0.1", "sp1.-500.1"]
)
def test_a_foreign_cursor_is_invalid(cursor: str) -> None:
    """A cursor this helper did not write asks the host to restart the pass.

    Args:
        cursor: The cursor sent.
    """

    async def fetch(_cursor: str, _params: Any) -> StorePage:
        return {"books": []}

    with pytest.raises(PluginError) as err:
        asyncio.run(scan_page(_params(cursor), fetch))
    assert err.value.code == "invalid_cursor"


def test_a_shrunk_store_page_is_invalid() -> None:
    """A later slice of a store page that shrank restarts the pass."""

    async def fetch(_cursor: str, _params: Any) -> StorePage:
        return {"books": [_book(0)]}

    with pytest.raises(PluginError) as err:
        asyncio.run(scan_page(_params(f"sp1.{MAX_SCAN_PAGE_BOOKS}."), fetch))
    assert err.value.code == "invalid_cursor"


def test_a_store_cursor_too_long_fails() -> None:
    """A store cursor that cannot fit a scan cursor fails before a reply."""

    async def fetch(_cursor: str, _params: Any) -> StorePage:
        return {"books": [], "next": "x" * MAX_SCAN_CURSOR_BYTES}

    with pytest.raises(PluginError) as err:
        asyncio.run(scan_page(_params(), fetch))
    assert err.value.code == "internal"


def test_cursors_match_the_typescript_sdk() -> None:
    """A cursor names its store cursor and offset as the TypeScript SDK does."""
    store = "a.b.é"

    async def fetch(cursor: str, _params: Any) -> StorePage:
        if cursor == "":
            return {"books": [_book(i) for i in range(MAX_SCAN_PAGE_BOOKS + 1)], "next": store}
        assert cursor == store
        return {"books": [_book(0)]}

    first = asyncio.run(scan_page(_params(), fetch))
    assert first["nextCursor"] == f"sp1.{MAX_SCAN_PAGE_BOOKS}."
    second = asyncio.run(scan_page(_params(first["nextCursor"]), fetch))
    assert second["nextCursor"] == f"sp1.0.{store}"
    assert asyncio.run(scan_page(_params(second["nextCursor"]), fetch))["nextCursor"] == ""
