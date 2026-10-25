#!/usr/bin/env python3
"""Adds report ID lines to the holding issue for untriaged reports.

Reports no triage plan covers are kept in one open issue,
``[diagnostics] untriaged reports``, instead of a new issue per run. Each run
appends the IDs it could not place to that issue's hidden report block,
which the dashboard reads (``dashboard/report-ids.js``: only the last
``<!-- bookclerk-diagnostics: report ids`` line, the bare ID lines directly
under it and the ``-->`` line that must close them). A holding issue filed
before the block existed lists its IDs under a visible ``### Report IDs``
heading; it is not edited, so the caller opens a new holding issue.

The body comes from GitHub, where anyone with triage rights can edit it, so
it is untrusted input: this module parses it sans-IO (``append_ids``), and
``scripts/tests/test_holding_issue.py`` replays a seed corpus and runs
seeded mutation fuzzing against an independent oracle.

Usage: ``holding_issue.py BODY_FILE LINES_FILE`` prints the new body. Exit
status 1 means the body cannot take the lines (no closed block, too long, or
full) and the caller should open a fresh holding issue.
"""

from __future__ import annotations

import pathlib
import re
import sys

#: Longest body read or written, in characters (GitHub allows 65,536).
MAX_BODY_CHARS = 60_000
#: Most IDs one holding issue lists, the dashboard parser's cap.
MAX_IDS = 500
#: Opening line of the block; only the last one in the body counts. Mirrors
#: ``REPORT_BLOCK_OPEN`` in ``dashboard/report-ids.js``.
BLOCK_OPEN = "<!-- bookclerk-diagnostics: report ids"
#: Closing line of the block.
BLOCK_CLOSE = "-->"
#: One ID line, trailing spaces and tabs allowed. Mirrors ``BLOCK_LINE_RE`` in
#: ``dashboard/report-ids.js``; the set has no ``>``, so an ID cannot close
#: the comment.
LINE_RE = re.compile(r"^([A-Za-z0-9._:/+-]{1,160})[ \t]*$")


class BodyError(Exception):
    """The body cannot take more IDs; the caller opens a new holding issue."""


def parse_lines(text: str) -> list[str]:
    """Reads the ID lines the caller wants to add.

    Args:
        text: One candidate ID per line, as ``triage-and-file.sh`` formats
            them.

    Returns:
        The IDs of the lines that match ``LINE_RE``, in order and without
        duplicates; other lines are dropped.
    """
    out: list[str] = []
    seen: set[str] = set()
    for raw in text.splitlines():
        m = LINE_RE.match(raw.rstrip("\r"))
        if m and m.group(1) not in seen:
            seen.add(m.group(1))
            out.append(m.group(1))
    return out


def append_ids(body: str, lines_text: str) -> str:
    """Appends ID lines to the last report block of a body.

    Args:
        body: The holding issue's current body.
        lines_text: Candidate lines, one per line (see ``parse_lines``).

    Returns:
        The body with the new IDs (those not already listed), one per
        line, added after the last listed ID, everything else unchanged.
        Line endings are normalized to ``\\n``.

    Raises:
        BodyError: The body is longer than ``MAX_BODY_CHARS``, has no
            report block whose ID lines end on its closing line, or would
            list more than ``MAX_IDS`` IDs.
    """
    if len(body) > MAX_BODY_CHARS:
        raise BodyError("body too long")
    lines = body.replace("\r\n", "\n").split("\n")
    block = max((i for i, line in enumerate(lines) if line.rstrip(" \t") == BLOCK_OPEN), default=-1)
    if block < 0:
        raise BodyError("no report block")
    listed: list[str] = []
    end = block + 1
    while end < len(lines):
        m = LINE_RE.match(lines[end])
        if not m:
            break
        listed.append(m.group(1))
        end += 1
    if end == len(lines) or lines[end].rstrip(" \t") != BLOCK_CLOSE:
        raise BodyError("report block is not closed")
    new = [rid for rid in parse_lines(lines_text) if rid not in set(listed)]
    if len(set(listed)) + len(new) > MAX_IDS:
        raise BodyError("holding issue is full")
    out = "\n".join(lines[:end] + new + lines[end:])
    if len(out) > MAX_BODY_CHARS:
        raise BodyError("body would be too long")
    return out


def main(argv: list[str]) -> int:
    """Prints the body with the lines appended.

    Args:
        argv: ``[BODY_FILE, LINES_FILE]``.

    Returns:
        0 with the new body on stdout; 1 when the body cannot take the lines
        (the reason goes to stderr); 2 on bad usage.
    """
    if len(argv) != 2:
        print("usage: holding_issue.py BODY_FILE LINES_FILE", file=sys.stderr)
        return 2
    body = pathlib.Path(argv[0]).read_text(encoding="utf-8", errors="replace")
    lines = pathlib.Path(argv[1]).read_text(encoding="utf-8", errors="replace")
    try:
        sys.stdout.write(append_ids(body, lines))
    except BodyError as err:
        print(err, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
