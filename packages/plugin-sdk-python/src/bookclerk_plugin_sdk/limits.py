"""Typed ``limit_exceeded`` errors: the SQL-v1 cap a request or result broke.

The host and every SDK format a limit error through
:func:`limit_exceeded_message` and parse it back through
:func:`parse_limit_exceeded`, so the record survives a plain-string hop
(for example through an engine error) and the ``details`` JSON on the wire.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any

_PREFIX = "limit exceeded: "
_BODY = re.compile(r"^([A-Za-z0-9_]+) is (\d+); used (\d+)")


@dataclass(frozen=True)
class LimitExceeded:
    """A negotiated cap that a request or its result exceeded.

    Attributes:
        limit: Capability field name as advertised (``maxResultBytes``,
            ``maxRequestBytes``, ...).
        cap: The negotiated cap.
        used: The observed value that exceeded ``cap``.
    """

    limit: str
    cap: int
    used: int


def limit_exceeded_message(limit: LimitExceeded) -> str:
    """Format the canonical message (``limit exceeded: <name> is <cap>; used <n>``).

    Args:
        limit: Exceeded cap.

    Returns:
        Message text the host and every SDK parse back.
    """
    return f"{_PREFIX}{limit.limit} is {limit.cap}; used {limit.used}"


def parse_limit_exceeded(message: str) -> LimitExceeded | None:
    """Parse a message produced by :func:`limit_exceeded_message`.

    Args:
        message: Error text, possibly wrapping the canonical message.

    Returns:
        The exceeded cap, or ``None`` when the text is not a limit error.
    """
    start = message.find(_PREFIX)
    if start < 0:
        return None
    match = _BODY.match(message[start + len(_PREFIX) :])
    if match is None:
        return None
    return LimitExceeded(match.group(1), int(match.group(2)), int(match.group(3)))


def limit_exceeded_details(limit: LimitExceeded) -> dict[str, Any]:
    """Build the ``details`` object (``{"limit", "cap", "used"}``).

    Args:
        limit: Exceeded cap.

    Returns:
        JSON-compatible detail object.
    """
    return {"limit": limit.limit, "cap": limit.cap, "used": limit.used}


def limit_from_details(details: dict[str, Any] | None) -> LimitExceeded | None:
    """Read a :class:`LimitExceeded` back from a ``details`` object.

    Args:
        details: Parsed ``details`` JSON object, or ``None``.

    Returns:
        The record, or ``None`` when a field is missing or mistyped.
    """
    if not details:
        return None
    limit = details.get("limit")
    cap = details.get("cap")
    used = details.get("used")
    if not isinstance(limit, str) or isinstance(cap, bool) or isinstance(used, bool):
        return None
    if not isinstance(cap, int) or not isinstance(used, int):
        return None
    return LimitExceeded(limit, cap, used)


__all__ = [
    "LimitExceeded",
    "limit_exceeded_details",
    "limit_exceeded_message",
    "limit_from_details",
    "parse_limit_exceeded",
]
