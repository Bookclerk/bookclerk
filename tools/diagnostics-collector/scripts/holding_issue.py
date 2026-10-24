#!/usr/bin/env python3
"""Adds report ID lines to the holding issue for untriaged reports.

Reports no triage plan covers are kept in one open issue,
``[diagnostics] untriaged reports``, instead of a new issue per run. Each run
appends the IDs it could not place to that issue's ``### Report IDs``
section, which the dashboard reads (``dashboard/report-ids.js``: only the
lines directly under the last such heading, each `` - `<id>` `` with an
optional dashboard link).

The body comes from GitHub, where anyone with triage rights can edit it, so
it is untrusted input: this module parses it sans-IO (``append_ids``), and
``scripts/tests/test_holding_issue.py`` replays a seed corpus and runs
seeded mutation fuzzing against an independent oracle.

Usage: ``holding_issue.py BODY_FILE LINES_FILE`` prints the new body. Exit
status 1 means the body cannot take the lines (no section, too long, or
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
#: The heading the list sits under; only the last one in the body counts.
HEADING_RE = re.compile(r"^###\s+Report IDs\s*$")
#: One list line: the ID and, optionally, its dashboard link. Mirrors
#: ``LINE_RE`` in ``dashboard/report-ids.js``.
LINE_RE = re.compile(r"^[-*] `([A-Za-z0-9._:/+-]{1,160})`(?: · \[dashboard\]\([^\s()]{1,300}\))?\s*$")


class BodyError(Exception):
    """The body cannot take more IDs; the caller opens a new holding issue."""


def parse_lines(text: str) -> list[tuple[str, str]]:
    """Reads the ID lines the caller wants to add.

    Args:
        text: One candidate line per line, as ``triage-and-file.sh`` formats
            them.

    Returns:
        ``(id, line)`` pairs for the lines that match ``LINE_RE``, in order
        and without duplicate IDs; other lines are dropped.
    """
    out: list[tuple[str, str]] = []
    seen: set[str] = set()
    for raw in text.splitlines():
        line = raw.rstrip("\r")
        m = LINE_RE.match(line)
        if not m or m.group(1) in seen:
            continue
        seen.add(m.group(1))
        out.append((m.group(1), line.rstrip()))
    return out


def append_ids(body: str, lines_text: str) -> str:
    """Appends ID lines to the last Report IDs section of a body.

    Args:
        body: The holding issue's current body.
        lines_text: Candidate lines, one per line (see ``parse_lines``).

    Returns:
        The body with the new lines (IDs not already listed) added after the
        last listed ID, everything else unchanged. Line endings are
        normalized to ``\\n``.

    Raises:
        BodyError: The body is longer than ``MAX_BODY_CHARS``, has no
            ``### Report IDs`` heading, or would list more than ``MAX_IDS``
            IDs.
    """
    if len(body) > MAX_BODY_CHARS:
        raise BodyError("body too long")
    lines = body.replace("\r\n", "\n").split("\n")
    heading = max((i for i, line in enumerate(lines) if HEADING_RE.match(line)), default=-1)
    if heading < 0:
        raise BodyError("no Report IDs section")
    listed: list[str] = []
    end = heading + 1
    while end < len(lines):
        m = LINE_RE.match(lines[end])
        if not m:
            break
        listed.append(m.group(1))
        end += 1
    new = [line for rid, line in parse_lines(lines_text) if rid not in set(listed)]
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
