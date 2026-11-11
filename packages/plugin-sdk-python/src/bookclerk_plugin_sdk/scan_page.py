"""Paging helper for a storefront's ``scan``.

A store page can hold more titles than one reply may carry
(``MAX_SCAN_PAGE_BOOKS``). :func:`scan_page` fetches the store page the cursor
names, returns the next slice of it, and writes a cursor that names both the
store page and the slice, so the plugin pages once and stays stateless
between calls. Cursors match the TypeScript SDK's ``scanPage``.
"""

from __future__ import annotations

import re
from collections.abc import Awaitable, Callable
from typing import TypedDict

from ._abi import MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS
from .abi import ScanBook, ScanPage, ScanParams
from .workerd import PluginError

__all__ = ["FetchStorePage", "StorePage", "scan_page"]

_CURSOR_PREFIX = "sp1."
"""Prefix of every cursor :func:`scan_page` writes; bumps if the layout changes."""

_OFFSET = re.compile(r"(0|[1-9][0-9]{0,15})")
"""Offset digits a cursor carries: no sign, no leading zero."""


class _StorePageRequired(TypedDict):
    """Fields every :class:`StorePage` carries.

    Attributes:
        books: Titles on this store page, in the store's order; any number.
    """

    books: list[ScanBook]


class StorePage(_StorePageRequired, total=False):
    """One page of a storefront's own library listing, as the plugin fetched it.

    Attributes:
        removed: Product ids gone from the account since
            ``ScanParams.syncToken``, on a delta pass only; any number.
        next: The storefront's own cursor for the store page after this one;
            empty or absent when this is the last store page.
        syncToken: On the last store page, the token the host sends as
            ``ScanParams.syncToken`` on the account's next pass.
        complete: On the last store page, whether the pass listed every title
            the account holds (a full pass).
        storePages: Storefront requests this fetch took; 1 when absent.
    """

    removed: list[str]
    next: str
    syncToken: str
    complete: bool
    storePages: int


FetchStorePage = Callable[[str, ScanParams], Awaitable[StorePage]]
"""Fetches the store page a storefront cursor names (empty for the first)."""


def _write_cursor(store_cursor: str, offset: int) -> str:
    """Cursor naming ``offset`` into the store page ``store_cursor`` names.

    Args:
        store_cursor: The storefront's own cursor for the store page.
        offset: Index of the first title the next reply carries.

    Returns:
        The cursor, at most ``MAX_SCAN_CURSOR_BYTES`` UTF-8 bytes.

    Raises:
        PluginError: ``internal`` when the store cursor is too long to fit.
    """
    cursor = f"{_CURSOR_PREFIX}{offset}.{store_cursor}"
    if len(cursor.encode()) > MAX_SCAN_CURSOR_BYTES:
        raise PluginError(
            "internal",
            f"store cursor too long: a scan cursor is at most {MAX_SCAN_CURSOR_BYTES} bytes",
        )
    return cursor


def _read_cursor(cursor: str) -> tuple[str, int]:
    """Store cursor and offset a scan cursor names.

    Args:
        cursor: ``ScanParams.cursor``; empty starts a pass.

    Returns:
        The storefront's cursor (empty for the first store page) and the
        offset into that store page.

    Raises:
        PluginError: ``invalid_cursor`` for a cursor this helper did not
            write, so the host restarts the pass.
    """
    if cursor == "":
        return "", 0
    stale = PluginError("invalid_cursor", "scan cursor was not written by scan_page")
    if not cursor.startswith(_CURSOR_PREFIX):
        raise stale
    digits, dot, store_cursor = cursor[len(_CURSOR_PREFIX) :].partition(".")
    if not dot or not _OFFSET.fullmatch(digits):
        raise stale
    offset = int(digits)
    if offset % MAX_SCAN_PAGE_BOOKS != 0:
        raise stale
    return store_cursor, offset


async def scan_page(params: ScanParams, fetch_store_page: FetchStorePage) -> ScanPage:
    """Answers one ``scan`` call from the storefront's own pages.

    Fetches the store page ``params["cursor"]`` names and returns at most
    ``MAX_SCAN_PAGE_BOOKS`` of its titles and removed ids. A larger store page
    is returned over several calls, each fetching it again. The pass ends on
    the last slice of the store page whose ``next`` is empty, which carries
    that page's ``syncToken`` and ``complete``.

    Args:
        params: The scan call's params.
        fetch_store_page: Fetches one store page from its storefront cursor.

    Returns:
        The reply for this call.

    Raises:
        PluginError: ``invalid_cursor`` when the cursor was not written by
            this helper or no longer fits the store page (the store changed
            mid-pass), so the host restarts the pass; ``internal`` when a
            store cursor is too long for a scan cursor.
    """
    store_cursor, offset = _read_cursor(params["cursor"])
    page = await fetch_store_page(store_cursor, params)
    books = page["books"]
    removed = page.get("removed", [])
    if offset > 0 and offset >= len(books) and offset >= len(removed):
        raise PluginError("invalid_cursor", "the store page shrank during the pass")
    end = offset + MAX_SCAN_PAGE_BOOKS
    reply: ScanPage = {
        "books": books[offset:end],
        "removed": removed[offset:end],
        "nextCursor": "",
        "syncToken": "",
        "complete": False,
        "storePages": page.get("storePages", 1),
    }
    if len(books) > end or len(removed) > end:
        reply["nextCursor"] = _write_cursor(store_cursor, end)
    elif page.get("next"):
        reply["nextCursor"] = _write_cursor(page.get("next", ""), 0)
    else:
        reply["syncToken"] = page.get("syncToken", "")
        reply["complete"] = page.get("complete", False)
    return reply
