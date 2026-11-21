#!/usr/bin/env python3
"""Reads the visual-baselines lock and validates a screenshot set.

The web app's visual checks compare screenshots with baselines kept in the
separate ``Bookclerk/bookclerk-visual-baselines`` repository
(docs/contributing/ui/visual-testing.md). Two inputs cross a trust boundary
here, so both are parsed sans-IO:

- ``ui/visual/baselines.lock``, which a pull request (possibly from a fork)
  edits. Its value becomes a git ref the CI job checks out, so only
  ``none`` or a 40-character lowercase commit id is accepted.
- The ``ui-visual`` artifact of a pull request's CI run. That run executes
  the pull request's code, so the artifact's files are attacker-chosen. The
  label workflow commits them to the baselines repository with a write
  token, so only PNG screenshots with the names, sizes and folders the
  checks produce are accepted.

- An ``issue_comment`` ``created`` event whose body is
  ``@<bot> approve-visuals``,
  the approval of someone without write access, and the team membership
  answer that decides it.
- The ``issue_comment`` ``edited`` event of the bot's review comment. Anyone
  with write access can edit it, so a baselines set is approved only when
  the edit ticks the comment's one checkbox and changes nothing else, and
  the approval names the commit and set the bot wrote.

Usage:

- ``visual_baselines.py lock PATH`` prints ``sha=<commit>`` (or ``sha=``
  for ``none``) for ``$GITHUB_OUTPUT``.
- ``visual_baselines.py validate DIR`` exits 0 when ``DIR`` holds exactly a
  valid set, printing the file count; otherwise it exits 1 with the reason.
"""

from __future__ import annotations

import json
import os
import re
import stat
import struct
import sys
from pathlib import Path

#: Largest lock file read, in bytes.
MAX_LOCK_BYTES = 128

#: A lock value naming a baselines commit.
COMMIT_ID = re.compile(r"[0-9a-f]{40}", re.ASCII)

#: Lock value for "no baselines yet": the check records instead of comparing.
LOCK_NONE = "none"

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


def parse_lock(raw: bytes) -> str | None:
    """Returns the baselines commit a lock file names.

    # Arguments
    * ``raw``: the lock file's bytes.

    # Returns
    The 40-character commit id, or ``None`` when the lock says ``none``.

    # Errors
    ``VisualBaselinesError`` when ``raw`` is too long, not ASCII, or not
    exactly one of the two values followed by at most one newline.
    """
    if len(raw) > MAX_LOCK_BYTES:
        raise VisualBaselinesError("lock file too long")
    try:
        text = raw.decode("ascii")
    except UnicodeDecodeError as err:
        raise VisualBaselinesError("lock file is not ASCII") from err
    text = text.removesuffix("\n")
    if text == LOCK_NONE:
        return None
    if COMMIT_ID.fullmatch(text) is None:
        raise VisualBaselinesError(
            "lock must be `none` or a 40-character lowercase commit id"
        )
    return text


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
    approved: dict[str, bytes], proposed: dict[str, bytes]
) -> list[tuple[str, str]]:
    """Lists how ``proposed`` differs from ``approved``.

    Screenshots are compared by bytes: the pinned image encodes the same
    pixels to the same PNG.

    # Arguments
    * ``approved``: the baselines the pull request's lock names, by path.
    * ``proposed``: the set its CI run recorded, by path.

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
        elif approved[path] != proposed[path]:
            changes.append((path, "changed"))
    return changes


def render_comment(
    head: str,
    set_commit: str,
    approved_commit: str | None,
    changes: list[tuple[str, str]],
    raw_base: str,
    bot_login: str,
) -> str:
    """Renders the bot's review comment for a proposed set.

    One comment holds every change: a summary line with the count in each
    category, the approval checkbox, then one collapsed section per
    category. Changed screenshots show the approved and proposed image, new
    ones only the proposed and removed ones only the approved. Only a set
    too large for one GitHub comment loses images: the rows that no longer
    fit list the screenshot's name alone.

    # Arguments
    * ``head``: the pull request head the set was recorded from.
    * ``set_commit``: the baselines commit holding the proposed set.
    * ``approved_commit``: the approved set the pull request is compared
      with, or ``None`` before any set was approved.
    * ``changes``: from ``diff_sets``; validated paths only.
    * ``raw_base``: the raw-file URL prefix of the baselines repository,
      such as ``https://raw.githubusercontent.com/<owner>/<repo>``.
    * ``bot_login``: the automation App's login, for the reply command.

    # Returns
    The comment body, opened by ``MARKER``.
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
            f"{len(changes)} screenshot(s) differ from the approved baselines at "
            f"`{head[:12]}`: {counts}."
        ),
        "",
        checkbox(head, False),
        "",
        f"Without write access, a visual reviewer approves by replying `{approve_command(bot_login)}`.",
        "",
    ]
    images = {
        "Approved": f"{raw_base}/{approved_commit}" if approved_commit else None,
        "Proposed": f"{raw_base}/{set_commit}",
    }
    sections: list[tuple[list[str], list[str], list[str]]] = []
    for kind, title, columns in CATEGORIES:
        paths = by_kind[kind]
        if not paths:
            continue
        top = [
            f"<details><summary>{title} ({len(paths)})</summary>",
            "",
            "| Screenshot | " + " | ".join(columns) + " |",
            "| --- |" + " --- |" * len(columns),
        ]
        rows = []
        for path in paths:
            cells = [
                f"![{column.lower()}]({images[column]}/{path})"
                if images[column]
                else "none"
                for column in columns
            ]
            rows.append(f"| `{path[:-4]}` | " + " | ".join(cells) + " |")
        sections.append(
            (top, rows, [f"| `{path[:-4]}` |" + " |" * len(columns) for path in paths])
        )
    tail = ["</details>", ""]
    size = sum(len(line) + 1 for line in head_lines)
    size += sum(
        sum(len(line) + 1 for line in top + tail) + sum(len(r) + 1 for r in bare)
        for top, _, bare in sections
    )
    lines = list(head_lines)
    for top, rows, bare in sections:
        lines += top
        for row, plain in zip(rows, bare, strict=True):
            # Swap a bare row for its imaged one while the whole body fits.
            if size - len(plain) + len(row) <= MAX_COMMENT_CHARS:
                size += len(row) - len(plain)
                lines.append(row)
            else:
                lines.append(plain)
        lines += tail
    return "\n".join(lines)


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
    request by ticking its checkbox and changing nothing else; ``None`` for
    any other edit, which the workflow ignores.

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


def main(argv: list[str]) -> int:
    """Runs the ``lock`` or ``validate`` command.

    # Arguments
    * ``argv``: the command and its path.

    # Returns
    The exit status.
    """
    if len(argv) != 2 or argv[0] not in ("lock", "validate"):
        print("usage: visual_baselines.py lock PATH | validate DIR", file=sys.stderr)
        return 2
    command, path = argv
    try:
        if command == "lock":
            sha = parse_lock(Path(path).read_bytes()[: MAX_LOCK_BYTES + 1])
            print(f"sha={sha or ''}")
        else:
            print(validate_set(read_set(Path(path))))
    except (VisualBaselinesError, OSError) as err:
        print(f"visual_baselines: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
