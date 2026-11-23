#!/usr/bin/env python3
"""Reads the visual-baselines lock and validates a screenshot set.

The web app's visual checks compare screenshots with baselines kept in the
separate ``Bookclerk/bookclerk-visual-baselines`` repository
(docs/contributing/ui/visual-testing.md). Two inputs cross a trust boundary
here, so both are parsed sans-IO:

- ``ui/visual/baselines.lock``, which a pull request (possibly from a fork)
  edits. Each entry names a screenshot path and the commit CI fetches its
  image from, so only sorted ``<path> <commit>`` lines with paths the checks
  produce and 40-character lowercase commit ids are accepted, and CI
  fetches only commits the approval workflow tagged.
- The ``ui-visual`` artifact of a pull request's CI run. That run executes
  the pull request's code, so the artifact's files are attacker-chosen. The
  bot commits its images to the baselines repository with a write token, so
  only PNG screenshots with the names, sizes and folders the checks produce
  are accepted. The comparison run's report and failure images only narrow
  which differences are proposed (``compare_finished``,
  ``compare_mismatches``); they never approve anything.

- The ``recheck-targets`` artifact of a visual recheck run
  (``.github/workflows/visual-recheck.yml``): the pull requests, heads and
  merge commits the run compared, which the bot proposes for. A job that
  runs no pull request code writes it before any job that does starts, but
  it is still read strictly (``parse_targets``).
- An ``issue_comment`` ``created`` event whose body is
  ``@<bot> approve-visuals``,
  the approval of someone without write access, and the team membership
  answer that decides it.
- The ``issue_comment`` ``edited`` event of the bot's review comment. Anyone
  with write access can edit it, so a baselines set is approved only when
  the edit ticks the comment's one checkbox and changes nothing else, and
  the approval names the commit and set the bot wrote.

Usage:

- ``visual_baselines.py assemble LOCK REPOSITORY_URL DIR`` fetches the
  approved image of every screenshot ``LOCK`` lists into ``DIR`` and prints
  ``dir=<DIR>`` (or ``dir=`` when the lock lists none) for
  ``$GITHUB_OUTPUT``.
- ``visual_baselines.py validate DIR`` exits 0 when ``DIR`` holds exactly a
  valid set, printing the file count; otherwise it exits 1 with the reason.
"""

from __future__ import annotations

import json
import os
import re
import stat
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

#: Largest lock file read, in bytes: room for every screenshot a set may
#: hold (``MAX_FILES``), each a path, a commit id and a blank line.
MAX_LOCK_BYTES = 1024 * 1024

#: A set commit of the baselines repository.
COMMIT_ID = re.compile(r"[0-9a-f]{40}", re.ASCII)

#: The comment the lock starts with, which ``render_lock`` writes. Comment
#: lines may only come before the first entry.
LOCK_HEADER = (
    "# Approved visual baselines (docs/contributing/ui/visual-testing.md).\n"
    "# One entry per screenshot: its path and the approved set commit of the\n"
    "# baselines repository that holds its image. The approval workflow\n"
    "# writes it. Entries stay sorted and apart, so pull requests that change\n"
    "# different screenshots merge without conflicts.\n"
)

#: Viewport folders the checks write, one per Playwright project.
VIEWPORTS = ("desktop", "phone")

#: A baseline's path inside a set: ``<viewport>/<story-id>--<theme>.png``.
#: Story ids are Storybook's kebab-case ids (letters, digits, single dashes);
#: a Worker page uses ``<worker>--<page>`` in their place (ui/visual/workers.spec.ts).
BASELINE_PATH = re.compile(
    r"(desktop|phone)/([a-z0-9]+(?:-{1,2}[a-z0-9]+)*)--(light|dark)\.png",
    re.ASCII,
)

#: Most files a set may hold.
MAX_FILES = 5000

#: Largest single screenshot, in bytes.
MAX_FILE_BYTES = 2 * 1024 * 1024

#: Largest whole set, in bytes.
MAX_SET_BYTES = 200 * 1024 * 1024

#: Largest image side, in pixels: Chromium's own screenshot limit. The
#: longest full-page website screenshots at the phone viewport are about
#: half of it.
MAX_SIDE = 16384

#: The eight bytes every PNG file starts with.
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


class VisualBaselinesError(ValueError):
    """The lock or the screenshot set is not acceptable."""


def parse_lock(raw: bytes) -> dict[str, str]:
    """Returns the approved set commit each screenshot is locked to.

    The lock lists one screenshot per line as ``<path> <commit>``, sorted by
    path with blank lines between entries (so git merges changes to
    different screenshots cleanly). Lines starting with ``#`` are comments
    and may only come before the first entry. A lock with no entries means
    no screenshot has been approved yet.

    # Arguments
    * ``raw``: the lock file's bytes.

    # Returns
    Each locked path (as ``BASELINE_PATH`` allows) and its 40-character
    lowercase commit id.

    # Errors
    ``VisualBaselinesError`` when ``raw`` is too long, not ASCII, holds a
    line that is neither blank, a leading comment nor an entry, or lists a
    path twice or out of order.
    """
    if len(raw) > MAX_LOCK_BYTES:
        raise VisualBaselinesError("lock file too long")
    try:
        text = raw.decode("ascii")
    except UnicodeDecodeError as err:
        raise VisualBaselinesError("lock file is not ASCII") from err
    entries: dict[str, str] = {}
    last = ""
    for number, line in enumerate(text.split("\n"), start=1):
        if line == "":
            continue
        if line.startswith("#"):
            if entries:
                raise VisualBaselinesError(
                    f"lock line {number}: comment after an entry"
                )
            continue
        path, sep, commit = line.partition(" ")
        if (
            not sep
            or BASELINE_PATH.fullmatch(path) is None
            or COMMIT_ID.fullmatch(commit) is None
        ):
            raise VisualBaselinesError(
                f"lock line {number} must be `<viewport>/<name>--<theme>.png <commit>`"
            )
        if path <= last:
            raise VisualBaselinesError(
                f"lock line {number}: {path} is repeated or out of order"
            )
        if len(entries) >= MAX_FILES:
            raise VisualBaselinesError("lock lists too many screenshots")
        entries[path] = commit
        last = path
    return entries


def render_lock(entries: dict[str, str]) -> str:
    """Writes a lock file ``parse_lock`` reads back as ``entries``.

    # Arguments
    * ``entries``: each screenshot path and its approved set commit.

    # Returns
    The header, then each entry in path order, a blank line after each.
    """
    return LOCK_HEADER + "".join(
        f"\n{path} {entries[path]}\n" for path in sorted(entries)
    )


def png_size(data: bytes) -> tuple[int, int]:
    """Returns the width and height a PNG file's header declares.

    Only the signature and the leading ``IHDR`` chunk are read; the image is
    never decoded.

    # Arguments
    * ``data``: the whole file.

    # Returns
    ``(width, height)`` in pixels, each between 1 and ``MAX_SIDE``.

    # Errors
    ``VisualBaselinesError`` when the signature is missing, the first chunk
    is not a 13-byte ``IHDR``, or a side is zero or larger than ``MAX_SIDE``.
    """
    if len(data) < 33 or not data.startswith(PNG_SIGNATURE):
        raise VisualBaselinesError("not a PNG file")
    length, kind = struct.unpack(">I4s", data[8:16])
    if kind != b"IHDR" or length != 13:
        raise VisualBaselinesError("PNG does not start with an IHDR chunk")
    width, height = struct.unpack(">II", data[16:24])
    if not (1 <= width <= MAX_SIDE and 1 <= height <= MAX_SIDE):
        raise VisualBaselinesError("PNG size out of range")
    return width, height


def validate_set(files: list[tuple[str, bytes]]) -> int:
    """Checks that ``files`` is a screenshot set the checks could produce.

    # Arguments
    * ``files``: each file's path relative to the set's root, using ``/``,
      and its bytes.

    # Returns
    The number of files.

    # Errors
    ``VisualBaselinesError`` when the set is empty or too large, a path is
    not ``<viewport>/<story-id>--<theme>.png`` or appears twice, or a file
    is too large or not a PNG of a plausible size.
    """
    if not files:
        raise VisualBaselinesError("empty set")
    if len(files) > MAX_FILES:
        raise VisualBaselinesError("too many files")
    seen: set[str] = set()
    total = 0
    for path, data in files:
        if BASELINE_PATH.fullmatch(path) is None:
            raise VisualBaselinesError(f"unexpected path {path!r}")
        if path in seen:
            raise VisualBaselinesError(f"duplicate path {path!r}")
        seen.add(path)
        if len(data) > MAX_FILE_BYTES:
            raise VisualBaselinesError(f"{path} is too large")
        total += len(data)
        if total > MAX_SET_BYTES:
            raise VisualBaselinesError("set too large")
        try:
            png_size(data)
        except VisualBaselinesError as err:
            raise VisualBaselinesError(f"{path}: {err}") from err
    return len(files)


#: Repository roles that may always approve a baselines set (the
#: ``role_name`` of ``GET /collaborators/{login}/permission``).
APPROVER_ROLES = frozenset({"admin", "maintain"})

#: The organization team whose active members may approve, with or
#: without write access (a designer, say).
APPROVER_TEAM = "visual-reviewers"

#: The word of the reply that approves the pending set for someone who
#: cannot tick the bot's checkbox (that needs write access). The reply
#: mentions the bot first, like the CLA check's ``recheck``:
#: ``@bookclerk approve-visuals`` (see ``approve_command``).
APPROVE_WORD = "approve-visuals"

#: Every ``role_name`` GitHub returns for a collaborator (custom roles
#: aside, which never approve).
KNOWN_ROLES = frozenset({"admin", "maintain", "write", "triage", "read", "none"})

#: The first line of the bot's review comment: the pull request head the
#: screenshots came from and the baselines commit that holds them.
MARKER = re.compile(
    r"<!-- bookclerk-visual-baselines v1 head=([0-9a-f]{40}) set=([0-9a-f]{40}) -->",
    re.ASCII,
)

#: The first line of a continuation comment: the images of the review
#: comment for ``set`` that did not fit in it, page ``part`` (from 1).
MORE_MARKER = re.compile(
    r"<!-- bookclerk-visual-baselines-more v1 set=([0-9a-f]{40}) part=([1-9][0-9]{0,3}) -->",
    re.ASCII,
)

#: Most characters a comment body holds; GitHub refuses bodies over 65,536,
#: and the rest is headroom for the outcome notes ``approve`` appends.
MAX_COMMENT_CHARS = 60_000

#: The categories of a change, in the order the comment shows them: the
#: section title and the image columns each row has.
CATEGORIES = (
    ("changed", "Changed", ("Approved", "Proposed")),
    ("added", "New", ("Proposed",)),
    ("removed", "Removed", ("Approved",)),
)

#: Largest event or API document read, in bytes.
MAX_DOCUMENT_BYTES = 4 * 1024 * 1024


def checkbox(head: str, checked: bool) -> str:
    """Returns the approval checkbox line for ``head``.

    # Arguments
    * ``head``: the pull request head commit the comment is for.
    * ``checked``: whether the box is ticked.

    # Returns
    The Markdown task-list line, the same for every render of ``head``.
    """
    return f"- [{'x' if checked else ' '}] Approve these baselines for `{head[:12]}` (maintainers and visual reviewers)"


def diff_sets(
    approved: dict[str, bytes],
    proposed: dict[str, bytes],
    mismatched: set[str] | None = None,
) -> list[tuple[str, str]]:
    """Lists how ``proposed`` differs from ``approved``.

    The pinned browser encodes the same pixels to the same PNG, but it does
    not always draw anti-aliased edges the same way twice. The comparison
    run allows for that (Playwright's per-pixel threshold), so when its
    mismatches are known, an image whose bytes differ only counts as changed
    if that run also found it different.

    # Arguments
    * ``approved``: the baselines the pull request's lock names, by path.
    * ``proposed``: the set its CI run recorded, by path.
    * ``mismatched``: the paths the run's comparison failed
      (``compare_mismatches``), or ``None`` when it did not compare, so any
      difference in bytes counts.

    # Returns
    ``(path, change)`` pairs sorted by path, ``change`` being ``added``,
    ``removed`` or ``changed``.
    """
    changes = []
    for path in sorted(approved.keys() | proposed.keys()):
        if path not in approved:
            changes.append((path, "added"))
        elif path not in proposed:
            changes.append((path, "removed"))
        elif approved[path] != proposed[path] and (
            mismatched is None or path in mismatched
        ):
            changes.append((path, "changed"))
    return changes


#: A comparison run's image of a failed screenshot:
#: ``<test folder>--<viewport>[-retry<n>]/<name>--<theme>-actual.png``.
COMPARE_ACTUAL = re.compile(
    r"[^/]*--(desktop|phone)(?:-retry[0-9]+)?/([a-z0-9]+(?:-{1,2}[a-z0-9]+)*--(?:light|dark))-actual\.png",
    re.ASCII,
)


def compare_mismatches(names: list[str]) -> set[str]:
    """Reads which screenshots a comparison run found different.

    Playwright writes ``<name>-actual.png`` into a failed test's output
    folder, whose name ends with the project (viewport).

    # Arguments
    * ``names``: the ``/``-separated paths under the run's ``test-results``
      folder.

    # Returns
    Each mismatched screenshot as a set path (``<viewport>/<name>.png``).
    """
    found = set()
    for name in names:
        match = COMPARE_ACTUAL.fullmatch(name)
        if match is not None:
            found.add(f"{match.group(1)}/{match.group(2)}.png")
    return found


#: Largest Playwright JSON report read, in bytes.
MAX_REPORT_BYTES = 64 * 1024 * 1024


def compare_finished(raw: bytes) -> int | None:
    """Reads how many tests failed in a comparison run that ran to the end.

    Its mismatches (``compare_mismatches``) only stand for every screenshot
    when it did: a run stopped by a setup error (a page server that never
    started, say) left the rest untested. Each test takes one screenshot, so
    the caller also checks that every failed test left a failed screenshot.

    # Arguments
    * ``raw``: the run's ``results.json``.

    # Returns
    The ``unexpected`` (failed) test count when the report is a JSON object
    with an empty ``errors`` list and ``stats`` counting at least one test
    that passed or failed; otherwise ``None``.
    """
    if len(raw) > MAX_REPORT_BYTES:
        return None
    try:
        report = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError):
        return None
    if not isinstance(report, dict) or report.get("errors") != []:
        return None
    stats = report.get("stats")
    if not isinstance(stats, dict):
        return None
    expected, unexpected = stats.get("expected"), stats.get("unexpected")
    if not (type(expected) is int and type(unexpected) is int):
        return None
    if expected < 0 or unexpected < 0 or expected + unexpected == 0:
        return None
    return unexpected


#: A line of a proposed set's commit message naming one change it carries.
CHANGE_LINE = re.compile(r"Change: (added|changed|removed) (\S+)", re.ASCII)

#: Kinds of change, as ``diff_sets`` names them.
CHANGE_KINDS = ("added", "changed", "removed")


def render_changes(changes: list[tuple[str, str]]) -> str:
    """Writes the change lines of a proposed set's commit message.

    The approval reads them back (``parse_changes``) to update only these
    screenshots' lock entries.

    # Arguments
    * ``changes``: from ``diff_sets``, sorted by path.

    # Returns
    One ``Change: <kind> <path>`` line per change.
    """
    return "".join(f"Change: {kind} {path}\n" for path, kind in changes)


def parse_changes(message: str) -> list[tuple[str, str]]:
    """Reads the changes a proposed set's commit message lists.

    # Arguments
    * ``message``: the set commit's message; lines that do not start with
      ``Change:`` are ignored.

    # Returns
    ``(path, kind)`` pairs in path order, as ``diff_sets`` returns them.

    # Errors
    ``VisualBaselinesError`` when the message is too long, a ``Change:``
    line names an unknown kind or an invalid path, a path repeats or is out
    of order, or there are more than ``MAX_FILES``.
    """
    if len(message) > MAX_LOCK_BYTES:
        raise VisualBaselinesError("set commit message too long")
    changes: list[tuple[str, str]] = []
    last = ""
    for line in message.split("\n"):
        if not line.startswith("Change:"):
            continue
        match = CHANGE_LINE.fullmatch(line)
        if match is None or BASELINE_PATH.fullmatch(match.group(2)) is None:
            raise VisualBaselinesError(f"malformed change line {line[:120]!r}")
        kind, path = match.groups()
        if path <= last:
            raise VisualBaselinesError(f"change to {path} is repeated or out of order")
        if len(changes) >= MAX_FILES:
            raise VisualBaselinesError("too many changes")
        changes.append((path, kind))
        last = path
    return changes


def apply_changes(
    lock: dict[str, str], set_commit: str, changes: list[tuple[str, str]]
) -> dict[str, str]:
    """Applies an approved set's changes to a lock.

    # Arguments
    * ``lock``: the branch's lock (``parse_lock``).
    * ``set_commit``: the approved set commit holding the new images.
    * ``changes``: its changes (``parse_changes``).

    # Returns
    A new lock: added and changed screenshots locked to ``set_commit``,
    removed ones dropped, every other entry as it was.
    """
    updated = dict(lock)
    for path, kind in changes:
        if kind == "removed":
            updated.pop(path, None)
        else:
            updated[path] = set_commit
    return updated


def render_comment(
    head: str,
    set_commit: str,
    approved: dict[str, str],
    changes: list[tuple[str, str]],
    raw_base: str,
    bot_login: str,
) -> str:
    """Renders the bot's review comment for a proposed set.

    The first body of ``render_comments``: see there.

    # Returns
    The comment body, opened by ``MARKER``.
    """
    return render_comments(head, set_commit, approved, changes, raw_base, bot_login)[0]


def render_comments(
    head: str,
    set_commit: str,
    approved: dict[str, str],
    changes: list[tuple[str, str]],
    raw_base: str,
    bot_login: str,
) -> list[str]:
    """Renders the bot's review comment for a proposed set, and the
    continuation comments that hold the images it has no room for.

    The review comment holds every change: a summary line with the count in
    each category, the approval checkbox, then one collapsed section per
    category. Changed screenshots show the approved and proposed image, new
    ones only the proposed and removed ones only the approved. A set too
    large for one GitHub comment lists the rows that no longer fit by name
    alone, says so under the summary, and shows their images in
    continuation comments (each opened by ``MORE_MARKER``), so every image
    can still be reviewed.

    # Arguments
    * ``head``: the pull request head the set was recorded from.
    * ``set_commit``: the baselines commit holding the proposed images.
    * ``approved``: the lock the pull request is compared with: each
      approved screenshot's path and the set commit holding its image.
    * ``changes``: from ``diff_sets``; validated paths only.
    * ``raw_base``: the raw-file URL prefix of the baselines repository,
      such as ``https://raw.githubusercontent.com/<owner>/<repo>``.
    * ``bot_login``: the automation App's login, for the reply command.

    # Returns
    The review comment's body, opened by ``MARKER``, then each continuation
    comment's body in order; each body at most ``MAX_COMMENT_CHARS``.
    """
    by_kind = {kind: [p for p, c in changes if c == kind] for kind, _, _ in CATEGORIES}
    counts = ", ".join(
        f"{len(by_kind[kind])} {title.lower()}"
        for kind, title, _ in CATEGORIES
        if by_kind[kind]
    )
    head_lines = [
        f"<!-- bookclerk-visual-baselines v1 head={head} set={set_commit} -->",
        "### Visual changes",
        "",
        (
            f"{len(changes)} screenshot(s) recorded at `{head[:12]}` differ from "
            f"the approved baselines: {counts}."
            if approved
            else f"{len(changes)} screenshot(s) recorded at `{head[:12]}`, with no "
            f"approved baselines to compare them with: {counts}."
        ),
        "",
        checkbox(head, False),
        "",
        f"Without write access, a visual reviewer approves by replying `{approve_command(bot_login)}`.",
        "",
    ]

    def image(column: str, path: str) -> str | None:
        """The raw URL of ``path``'s image in ``column``, if there is one."""
        if column == "Proposed":
            return f"{raw_base}/{set_commit}/{path}"
        commit = approved.get(path)
        return f"{raw_base}/{commit}/{path}" if commit else None

    sections: list[tuple[str, list[str], list[str], list[str], list[str]]] = []
    for kind, title, columns in CATEGORIES:
        paths = by_kind[kind]
        if not paths:
            continue
        header = [
            "| Screenshot | " + " | ".join(columns) + " |",
            "| --- |" + " --- |" * len(columns),
        ]
        top = [f"<details><summary>{title} ({len(paths)})</summary>", "", *header]
        rows = []
        for path in paths:
            urls = [(column, image(column, path)) for column in columns]
            cells = [f"![{c.lower()}]({url})" if url else "none" for c, url in urls]
            rows.append(f"| `{path[:-4]}` | " + " | ".join(cells) + " |")
        bare = [f"| `{path[:-4]}` |" + " |" * len(columns) for path in paths]
        sections.append((title, header, top, rows, bare))
    tail = ["</details>", ""]
    # Room for the note a set that overflows gets under its summary.
    note_room = 160
    size = sum(len(line) + 1 for line in head_lines) + note_room
    size += sum(
        sum(len(line) + 1 for line in top + tail) + sum(len(r) + 1 for r in bare)
        for _, _, top, _, bare in sections
    )
    lines = list(head_lines)
    overflow: list[tuple[str, list[str], list[str]]] = []
    for title, header, top, rows, bare in sections:
        lines += top
        left: list[str] = []
        for row, plain in zip(rows, bare, strict=True):
            # Swap a bare row for its imaged one while the whole body fits.
            if size - len(plain) + len(row) <= MAX_COMMENT_CHARS:
                size += len(row) - len(plain)
                lines.append(row)
            else:
                lines.append(plain)
                left.append(row)
        lines += tail
        if left:
            overflow.append((title, header, left))
    pages = _continuation_pages(set_commit, overflow)
    if pages:
        missing = sum(len(left) for _, _, left in overflow)
        lines.insert(
            len(head_lines) - 1,
            f"\n{missing} row(s) below have no room for their images here; "
            f"they are in the {len(pages)} comment(s) that follow.",
        )
    return ["\n".join(lines), *pages]


def _continuation_pages(
    set_commit: str, overflow: list[tuple[str, list[str], list[str]]]
) -> list[str]:
    """Lays the imaged rows the review comment had no room for out over
    continuation comments.

    # Arguments
    * ``set_commit``: the baselines commit holding the proposed set.
    * ``overflow``: per category with rows left over, its title, its table
      header and those rows, in the review comment's order.

    # Returns
    Each continuation comment's body, opened by ``MORE_MARKER`` and at most
    ``MAX_COMMENT_CHARS`` long; empty when nothing overflowed.
    """
    pages: list[list[str]] = []
    page: list[str] = []
    size = 0

    def start() -> None:
        nonlocal page, size
        page = [
            f"<!-- bookclerk-visual-baselines-more v1 set={set_commit} part={len(pages) + 1} -->",
            "### Visual changes, continued",
            "",
        ]
        pages.append(page)
        size = sum(len(line) + 1 for line in page)

    for title, header, rows in overflow:
        opened = False
        for row in rows:
            heading = [f"#### {title}", "", *header]
            need = (
                len(row) + 1 + (0 if opened else sum(len(h) + 1 for h in heading) + 1)
            )
            if not page or size + need > MAX_COMMENT_CHARS:
                start()
                opened = False
                need = len(row) + 1 + sum(len(h) + 1 for h in heading) + 1
            if not opened:
                if len(page) > 3:
                    page.append("")
                page.extend(heading)
                opened = True
            page.append(row)
            size += need
    return ["\n".join(p) for p in pages]


def parse_role(raw: bytes) -> str:
    """Reads the ``role_name`` of a ``GET /collaborators/{login}/permission`` answer.

    # Arguments
    * ``raw``: the response body.

    # Returns
    The role, one of ``KNOWN_ROLES``; a custom role reads as ``none``.

    # Errors
    ``VisualBaselinesError`` when the body is not an object with a string
    ``role_name``.
    """
    obj = _json_object(raw, "permission")
    role = obj.get("role_name")
    if not isinstance(role, str):
        raise VisualBaselinesError("permission has no role_name")
    return role if role in KNOWN_ROLES else "none"


def parse_membership(raw: bytes) -> bool:
    """Reads a ``GET /orgs/{org}/teams/{team}/memberships/{login}`` answer.

    # Arguments
    * ``raw``: the body of a 200 answer (a 404 means not a member).

    # Returns
    Whether the membership is ``active`` (a pending invitation is not).

    # Errors
    ``VisualBaselinesError`` when the body is not an object with a string
    ``state``.
    """
    obj = _json_object(raw, "membership")
    state = obj.get("state")
    if not isinstance(state, str):
        raise VisualBaselinesError("membership has no state")
    return state == "active"


def approve_command(bot_login: str) -> str:
    """Returns the approval reply for bot ``bot_login``, such as ``@bookclerk approve-visuals``.

    # Arguments
    * ``bot_login``: the App's login, ``<slug>[bot]``; it is mentioned by
      its slug.

    # Returns
    The command text.
    """
    return f"@{bot_login.removesuffix('[bot]')} {APPROVE_WORD}"


def parse_command(raw: bytes, bot_login: str) -> tuple[int, str] | None:
    """Reads an ``issue_comment`` ``created`` event as an approval command.

    # Arguments
    * ``raw``: the event payload.
    * ``bot_login``: the App's login, such as ``bookclerk[bot]``.

    # Returns
    ``(pull request number, sender login)`` when a user commented exactly
    ``approve_command(bot_login)`` (surrounding whitespace and ASCII case
    aside, like a mention) on a pull request;
    ``None`` for any other comment.

    # Errors
    ``VisualBaselinesError`` when the payload has the wrong shape.
    """
    event = _json_object(raw, "event")
    if event.get("action") != "created":
        return None
    comment = _field(event, "comment", dict, "comment")
    issue = _field(event, "issue", dict, "issue")
    sender = _field(event, "sender", dict, "sender")
    if sender.get("type") != "User" or not isinstance(issue.get("pull_request"), dict):
        return None
    body = _field(comment, "body", str, "comment body")
    text = body.strip()
    if not text.isascii() or text.lower() != approve_command(bot_login).lower():
        return None
    number = _field(issue, "number", int, "issue number")
    login = _field(sender, "login", str, "sender login")
    return (number, login) if number > 0 else None


class Approval:
    """A checkbox tick on the bot's review comment, from an ``edited`` event."""

    def __init__(
        self, number: int, comment_id: int, sender: str, head: str, set_commit: str
    ) -> None:
        """Holds the approval's fields.

        # Arguments
        * ``number``: the pull request number.
        * ``comment_id``: the review comment's id.
        * ``sender``: the login that ticked the box.
        * ``head``: the pull request head the comment is for.
        * ``set_commit``: the baselines commit the comment proposes.
        """
        #: The pull request number.
        self.number = number
        #: The review comment's id, to update it afterwards.
        self.comment_id = comment_id
        #: The login that ticked the box; its role decides.
        self.sender = sender
        #: The pull request head the comment is for.
        self.head = head
        #: The baselines commit the comment proposes.
        self.set_commit = set_commit


def _json_object(raw: bytes, what: str) -> dict[str, object]:
    """Decodes ``raw`` as one JSON object of at most ``MAX_DOCUMENT_BYTES``.

    # Errors
    ``VisualBaselinesError`` when it is too large, not JSON, or not an object.
    """
    if len(raw) > MAX_DOCUMENT_BYTES:
        raise VisualBaselinesError(f"{what} too large")
    try:
        value = json.loads(raw)
    except (ValueError, RecursionError) as err:
        raise VisualBaselinesError(f"{what} is not JSON") from err
    if not isinstance(value, dict):
        raise VisualBaselinesError(f"{what} is not an object")
    return value


def _field(obj: dict[str, object], key: str, kind: type, what: str):
    """Returns ``obj[key]`` when it has exactly type ``kind``.

    # Errors
    ``VisualBaselinesError`` otherwise.
    """
    value = obj.get(key)
    if type(value) is not kind:
        raise VisualBaselinesError(f"{what} is missing or malformed")
    return value


def parse_approval(raw: bytes, bot_login: str) -> Approval | None:
    """Reads an ``issue_comment`` ``edited`` event as an approval.

    # Arguments
    * ``raw``: the event payload.
    * ``bot_login``: the automation App's login, such as ``bookclerk[bot]``.

    # Returns
    The approval when a user edited the bot's review comment on a pull
    request by ticking its checkbox and changing nothing else (line endings
    aside: a tick saved from the browser comes back with CRLF ones);
    ``None`` for any other edit, which the workflow ignores.

    # Errors
    ``VisualBaselinesError`` when the payload has the wrong shape.
    """
    event = _json_object(raw, "event")
    if event.get("action") != "edited":
        return None
    comment = _field(event, "comment", dict, "comment")
    issue = _field(event, "issue", dict, "issue")
    author = _field(comment, "user", dict, "comment author")
    sender = _field(event, "sender", dict, "sender")
    if (
        _field(author, "login", str, "comment author") != bot_login
        or author.get("type") != "Bot"
    ):
        return None
    if sender.get("type") != "User" or not isinstance(issue.get("pull_request"), dict):
        return None
    login = _field(sender, "login", str, "sender login")
    number = _field(issue, "number", int, "issue number")
    comment_id = _field(comment, "id", int, "comment id")
    new = _field(comment, "body", str, "comment body")
    changes = event.get("changes")
    if not isinstance(changes, dict) or not isinstance(changes.get("body"), dict):
        return None
    old = _field(changes["body"], "from", str, "previous body")
    # Saving a tick from the browser sends the whole body back with CRLF
    # line endings, so line endings are not a change.
    new, old = new.replace("\r\n", "\n"), old.replace("\r\n", "\n")
    match = MARKER.match(new)
    if match is None or number <= 0 or comment_id <= 0:
        return None
    head, set_commit = match.groups()
    unticked, ticked = checkbox(head, False), checkbox(head, True)
    if (
        old.count(unticked) != 1
        or ticked in old
        or old.replace(unticked, ticked) != new
    ):
        return None
    return Approval(number, comment_id, login, head, set_commit)


#: Most pull requests one visual recheck run compares, so a burst of
#: queued pull requests never takes more than a few runners at a time.
MAX_RECHECK_TARGETS = 20

#: Largest pull request number accepted, GitHub's 32-bit id range.
MAX_PULL_NUMBER = 2**31 - 1


class RecheckTarget:
    """One pull request a visual recheck run compares with the default branch.

    # Fields
    * ``number``: the pull request.
    * ``head``: its head commit when the run was planned; a proposal is
      only made while the pull request is still at it.
    * ``merge``: the commit the run checked out, the head merged into the
      default branch commit the run was started for.
    """

    __slots__ = ("head", "merge", "number")

    def __init__(self, number: int, head: str, merge: str) -> None:
        """Holds the fields as given; ``parse_targets`` validates them."""
        #: The pull request number.
        self.number = number
        #: The head commit id the run planned with.
        self.head = head
        #: The merge commit id the run checked out.
        self.merge = merge

    def __eq__(self, other: object) -> bool:
        """Equal when every field is."""
        return isinstance(other, RecheckTarget) and (
            self.number,
            self.head,
            self.merge,
        ) == (other.number, other.head, other.merge)

    def __repr__(self) -> str:
        """``RecheckTarget(number, head, merge)``, for test failures."""
        return f"RecheckTarget({self.number}, {self.head!r}, {self.merge!r})"


def render_targets(base: str, targets: list[RecheckTarget]) -> str:
    """Writes the ``recheck-targets`` document ``parse_targets`` reads.

    # Arguments
    * ``base``: the default branch commit the run compares against.
    * ``targets``: the pull requests, in any order; written sorted by number.
    """
    return json.dumps(
        {
            "base": base,
            "targets": [
                {"number": t.number, "head": t.head, "merge": t.merge}
                for t in sorted(targets, key=lambda t: t.number)
            ],
        },
        sort_keys=True,
    )


def parse_targets(raw: bytes) -> tuple[str, list[RecheckTarget]]:
    """Reads a visual recheck run's ``recheck-targets`` document.

    It is a JSON object with exactly the keys ``base`` (a commit id) and
    ``targets``: a list of at most ``MAX_RECHECK_TARGETS`` objects with
    exactly the keys ``number`` (an integer from 1 to ``MAX_PULL_NUMBER``),
    ``head`` and ``merge`` (commit ids), in strictly increasing ``number``
    order. Commit ids are 40 lowercase hex characters.

    # Arguments
    * ``raw``: the document's bytes.

    # Returns
    The base commit and the targets.

    # Errors
    ``VisualBaselinesError`` when the document breaks any of those rules.
    """
    doc = _json_object(raw, "recheck targets")
    if set(doc) != {"base", "targets"}:
        raise VisualBaselinesError("recheck targets need exactly base and targets")
    base = _field(doc, "base", str, "recheck base")
    if COMMIT_ID.fullmatch(base) is None:
        raise VisualBaselinesError("recheck base is not a commit id")
    entries = _field(doc, "targets", list, "recheck target list")
    if len(entries) > MAX_RECHECK_TARGETS:
        raise VisualBaselinesError("too many recheck targets")
    out: list[RecheckTarget] = []
    for entry in entries:
        if not isinstance(entry, dict) or set(entry) != {"number", "head", "merge"}:
            raise VisualBaselinesError("recheck target is malformed")
        number = _field(entry, "number", int, "recheck target number")
        if not 1 <= number <= MAX_PULL_NUMBER:
            raise VisualBaselinesError("recheck target number is out of range")
        if out and number <= out[-1].number:
            raise VisualBaselinesError("recheck targets are not in order")
        head = _field(entry, "head", str, "recheck target head")
        merge = _field(entry, "merge", str, "recheck target merge")
        if COMMIT_ID.fullmatch(head) is None or COMMIT_ID.fullmatch(merge) is None:
            raise VisualBaselinesError("recheck target commit is not a commit id")
        out.append(RecheckTarget(number, head, merge))
    return base, out


def read_set(root: Path) -> list[tuple[str, bytes]]:
    """Reads every entry under ``root`` without following links.

    # Arguments
    * ``root``: the downloaded artifact's baselines folder.

    # Returns
    Each regular file's ``/``-separated relative path and bytes.

    # Errors
    ``VisualBaselinesError`` for a symbolic link, a special file, or more
    entries or bytes than a valid set may hold.
    """
    files: list[tuple[str, bytes]] = []
    total = 0
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        for name in [*dirnames, *filenames]:
            full = Path(dirpath, name)
            mode = full.lstat().st_mode
            if stat.S_ISLNK(mode):
                raise VisualBaselinesError(f"link {full.relative_to(root)}")
            if name in filenames and not stat.S_ISREG(mode):
                raise VisualBaselinesError(f"special file {full.relative_to(root)}")
        for name in filenames:
            full = Path(dirpath, name)
            size = full.lstat().st_size
            total += size
            if (
                len(files) >= MAX_FILES
                or size > MAX_FILE_BYTES
                or total > MAX_SET_BYTES
            ):
                raise VisualBaselinesError("set too large")
            files.append((full.relative_to(root).as_posix(), full.read_bytes()))
    return files


#: Largest ``git ls-remote`` answer read, in bytes.
MAX_LS_REMOTE_BYTES = 8 * 1024 * 1024


def approved_commits(raw: bytes) -> set[str]:
    """Reads the approved set commits from ``git ls-remote`` output.

    Only the approval workflow can create ``refs/tags/approved/<commit>``
    tags in the baselines repository (a ruleset keeps everyone else out), so
    a set commit is approved when such a tag names it and points at it.

    # Arguments
    * ``raw``: the output of ``git ls-remote <baselines> 'refs/tags/approved/*'``.

    # Returns
    Every commit whose ``approved/<commit>`` tag points at itself.

    # Errors
    ``VisualBaselinesError`` when ``raw`` is too long or not ASCII.
    """
    if len(raw) > MAX_LS_REMOTE_BYTES:
        raise VisualBaselinesError("ls-remote answer too long")
    try:
        text = raw.decode("ascii")
    except UnicodeDecodeError as err:
        raise VisualBaselinesError("ls-remote answer is not ASCII") from err
    found: set[str] = set()
    for line in text.split("\n"):
        target, sep, ref = line.partition("\t")
        commit = ref.removeprefix("refs/tags/approved/")
        if sep and COMMIT_ID.fullmatch(commit) and target == commit:
            found.add(commit)
    return found


def _git(args: list[str], cwd: Path | None = None) -> bytes:
    """Runs git with ``args`` and returns its standard output.

    # Errors
    ``VisualBaselinesError`` when git exits with an error.
    """
    done = subprocess.run(["git", *args], cwd=cwd, capture_output=True, check=False)
    if done.returncode != 0:
        raise VisualBaselinesError(
            f"git {args[0]} failed: {done.stderr.decode('utf-8', 'replace').strip()}"
        )
    return done.stdout


def assemble(lock: dict[str, str], repository_url: str, dest: Path) -> int:
    """Writes the approved image of every locked screenshot under ``dest``.

    # Arguments
    * ``lock``: the parsed lock (``parse_lock``).
    * ``repository_url``: the baselines repository, public, so no token.
    * ``dest``: an empty folder; each image lands at its lock path.

    # Returns
    The number of images written.

    # Errors
    ``VisualBaselinesError`` when a locked commit has no ``approved`` tag,
    git cannot fetch a commit or find a path in it, or an image is not a PNG
    the checks could have written.
    """
    if not lock:
        return 0
    commits = sorted(set(lock.values()))
    approved = approved_commits(
        _git(["ls-remote", repository_url, "refs/tags/approved/*"])
    )
    missing = [c for c in commits if c not in approved]
    if missing:
        raise VisualBaselinesError(
            "not approved baselines sets (no approved/<commit> tag): "
            + ", ".join(missing)
        )
    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp)
        _git(["init", "-q", "--bare"], repo)
        _git(["fetch", "-q", "--depth", "1", repository_url, *commits], repo)
        for path, commit in lock.items():
            data = _git(["cat-file", "blob", f"{commit}:{path}"], repo)
            if len(data) > MAX_FILE_BYTES:
                raise VisualBaselinesError(f"{path} is too large")
            png_size(data)
            (dest / path).parent.mkdir(parents=True, exist_ok=True)
            (dest / path).write_bytes(data)
    return len(lock)


def main(argv: list[str]) -> int:
    """Runs the ``assemble`` or ``validate`` command.

    # Arguments
    * ``argv``: the command and its paths.

    # Returns
    The exit status.
    """
    if not (
        (len(argv) == 4 and argv[0] == "assemble")
        or (len(argv) == 2 and argv[0] == "validate")
    ):
        print(
            "usage: visual_baselines.py assemble LOCK REPOSITORY_URL DIR | validate DIR",
            file=sys.stderr,
        )
        return 2
    try:
        if argv[0] == "assemble":
            lock_path, url, dest = argv[1], argv[2], Path(argv[3])
            lock = parse_lock(Path(lock_path).read_bytes()[: MAX_LOCK_BYTES + 1])
            dest.mkdir(parents=True, exist_ok=True)
            count = assemble(lock, url, dest)
            print(f"dir={dest if count else ''}")
        else:
            print(validate_set(read_set(Path(argv[1]))))
    except (VisualBaselinesError, OSError) as err:
        print(f"visual_baselines: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
