#!/usr/bin/env python3
"""Renders the summary comment the ingest run keeps on each diagnostics issue.

The collector's ``GET /issues/summaries`` answers, for every issue whose
figures changed since the last run, how many reports the issue tracks and
from how many installations, how they were linked, when the first and last
arrived, the versions and platforms seen and the newest reports. This module
turns each into one Markdown comment, marked on its first line so the run
finds and edits the same comment next time (editing notifies nobody), and
hands back the fingerprint the collector gave, which the run echoes to
``POST /issues/summary`` once the comment is written.

The figures are counts the collector computed, but the versions and
platforms in them come from reports anyone can submit, so every string is
validated or rewritten before it reaches a public comment: nothing in it can
carry a link, a mention or a reference of its own, and report IDs link only
to their own dashboard page on the given collector origin. The module is
sans-IO (``render_summaries``); ``scripts/tests/test_summary_comment.py``
replays a seed corpus and runs seeded mutation fuzzing against an
independent oracle. ``tools/diagnostics-collector/dashboard/summary.js`` is
the same renderer in JavaScript, for the dashboard's own comment update;
its fuzz test runs this module as the oracle, so the two cannot drift.

Usage: ``summary_comment.py SUMMARIES_FILE OUT_DIR [DASHBOARD_URL]``. Each
summary becomes ``OUT_DIR/<n>/{issue,fingerprint,body}``. Exits 0 with no
files when the answer holds no usable summary.
"""

from __future__ import annotations

import datetime
import importlib.util
import json
import pathlib
import re
import sys
import types
from dataclasses import dataclass


def _sibling(name: str) -> types.ModuleType:
    """Loads a module from this script's directory.

    ``python3 -I`` keeps the script's directory off ``sys.path`` so a planted
    module cannot shadow the standard library; the sibling is loaded by its
    own path instead.
    """
    path = pathlib.Path(__file__).resolve().with_name(f"{name}.py")
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


#: The triage plan validator: one copy of the issue-text sanitizer, the
#: dashboard origin check and the report line.
_vp = _sibling("validate_plan")
defang = _vp.defang
dashboard_origin = _vp.dashboard_origin
UUID_RE = _vp.UUID_RE

#: First line of every summary comment; the run edits the comment that
#: starts with it.
MARKER = "<!-- bookclerk-diagnostics: summary -->"
#: Most summaries one run renders (the collector hands out at most this many).
MAX_SUMMARIES = 20
#: Most versions or platforms listed (the collector folds the rest into "others").
MAX_BREAKDOWN = 9
#: Most newest reports listed.
MAX_NEWEST = 10
#: Longest comment, in characters, well inside GitHub's 65,536.
MAX_BODY_CHARS = 8000
#: A report ID as the collector names them (object name or UUID).
ID_RE = re.compile(r"[A-Za-z0-9._:-]{1,128}")
#: A version as reports carry it, or the collector's "others" bucket (ASCII
#: digits only, as in the dashboard's port of this renderer).
VERSION_RE = re.compile(r"v?[0-9]{1,6}\.[0-9]{1,6}(?:\.[0-9]{1,6})?(?:[-+._~][0-9A-Za-z.+_~-]{1,40})?")
#: A platform (``os arch``) as reports carry it.
PLATFORM_RE = re.compile(r"[A-Za-z0-9._-]{1,32}(?: [A-Za-z0-9._-]{1,32})?")
#: Whitespace trimmed from versions and collapsed in platforms: the ASCII
#: set, so the dashboard's port (`dashboard/summary.js`) agrees exactly.
ASCII_WS = " \t\n\r\x0b\x0c"
#: A collector fingerprint: lowercase hex SHA-256.
FINGERPRINT_RE = re.compile(r"[0-9a-f]{64}")
#: Largest issue number or count taken.
MAX_NUMBER = 999_999_999
#: Latest time rendered as a date (year 9999 is far past the last report).
MAX_TIME_MS = 253_402_300_799_000


@dataclass(frozen=True)
class Rendered:
    """One summary comment to write.

    Attributes:
        issue: The issue's number.
        fingerprint: The collector's fingerprint of the figures rendered.
        body: The comment, starting with ``MARKER``.
    """

    issue: int
    fingerprint: str
    body: str


def _count(value: object) -> int | None:
    """A count or issue number, or None for anything else."""
    if isinstance(value, bool) or not isinstance(value, int) or value < 0 or value > MAX_NUMBER:
        return None
    return value


def _day(value: object) -> str | None:
    """A UTC date for a time in milliseconds, or None."""
    if isinstance(value, bool) or not isinstance(value, int) or value < 0 or value > MAX_TIME_MS:
        return None
    return datetime.datetime.fromtimestamp(value / 1000, tz=datetime.timezone.utc).strftime("%Y-%m-%d")


def _version(value: object) -> str:
    """A version for the comment: validated and rewritten, else ``unknown``."""
    if isinstance(value, str):
        text = value.strip(ASCII_WS)
        if text == "others" or VERSION_RE.fullmatch(text):
            return defang(text)
    return "unknown"


def _platform(value: object) -> str:
    """A platform for the comment: validated and rewritten, else ``unknown``."""
    if isinstance(value, str):
        text = " ".join(part for part in re.split(f"[{ASCII_WS}]+", value) if part)
        if text == "others" or PLATFORM_RE.fullmatch(text):
            return defang(text)
    return "unknown"


def _breakdown(value: object, name: object) -> list[tuple[str, int]]:
    """The ``(label, count)`` pairs of a versions or platforms list."""
    if not isinstance(value, list):
        return []
    out: list[tuple[str, int]] = []
    for entry in value[:MAX_BREAKDOWN]:
        if not isinstance(entry, dict):
            continue
        n = _count(entry.get("n"))
        if n is None:
            continue
        label = _version(entry.get("version")) if name == "version" else _platform(entry.get("platform"))
        out.append((label, n))
    return out


def _plural(n: int, word: str) -> str:
    """``n word(s)``."""
    return f"{n} {word}{'' if n == 1 else 's'}"


def render_summary(entry: object, dashboard: str | None) -> Rendered | None:
    """Renders one issue's summary.

    Args:
        entry: One element of the collector's ``issues`` list.
        dashboard: Validated collector origin for report links, or None.

    Returns:
        The comment, or None when the entry lacks a valid issue number,
        fingerprint or report count.
    """
    if not isinstance(entry, dict):
        return None
    issue = _count(entry.get("issue"))
    fingerprint = entry.get("fingerprint")
    reports = _count(entry.get("reports"))
    if not issue or reports is None or not isinstance(fingerprint, str) or not FINGERPRINT_RE.fullmatch(fingerprint):
        return None
    installs = _count(entry.get("installs")) or 0
    first = _day(entry.get("first_seen"))
    last = _day(entry.get("last_seen"))
    sources = entry.get("sources") if isinstance(entry.get("sources"), dict) else {}
    counts = {k: _count(sources.get(k)) or 0 for k in ("filed", "signature", "triage", "manual")}
    lines = [MARKER]
    if reports == 0:
        lines.append("**Diagnostics summary** · no report is linked to this issue at present")
    else:
        head = f"**Diagnostics summary** · {_plural(reports, 'report')} from {_plural(installs, 'installation')}"
        if first:
            head += f" · first seen {first}"
        if last:
            head += f" · last seen {last}"
        lines.append(head)
        lines.append("")
        lines.append(
            f"- Linked: {counts['filed']} listed in this issue, {counts['signature']} matched by error signature, "
            f"{counts['triage']} added by triage, {counts['manual']} attached by members"
        )
        versions = _breakdown(entry.get("versions"), "version")
        if versions:
            lines.append("- Versions: " + ", ".join(f"{v} ({n})" for v, n in versions))
        platforms = _breakdown(entry.get("platforms"), "platform")
        if platforms:
            lines.append("- Platforms: " + ", ".join(f"{p} ({n})" for p, n in platforms))
        newest = entry.get("newest")
        rows: list[str] = []
        for r in (newest if isinstance(newest, list) else [])[:MAX_NEWEST]:
            if not isinstance(r, dict):
                continue
            rid = r.get("report_id")
            if not isinstance(rid, str) or not ID_RE.fullmatch(rid):
                continue
            row = f"- `{rid}` · {_version(r.get('version'))}"
            day = _day(r.get("received_at"))
            if day:
                row += f" · {day}"
            if dashboard and UUID_RE.fullmatch(rid):
                row += f" · [dashboard]({dashboard}/reports/{rid})"
            rows.append(row)
        if rows:
            lines.append("")
            lines.append("Newest reports (dashboard pages are for organization members; the full list is there):")
            lines.extend(rows)
    lines.append("")
    lines.append("_Kept up to date by the diagnostics ingest run and edited in place; report contents are never posted here._")
    body = "\n".join(lines) + "\n"
    if len(body) > MAX_BODY_CHARS:
        return None
    return Rendered(issue=issue, fingerprint=fingerprint, body=body)


def render_summaries(payload: object, dashboard: str | None = None) -> list[Rendered]:
    """Renders the collector's answer.

    Args:
        payload: The decoded ``GET /issues/summaries`` answer (an object with
            ``issues``) or its ``issues`` list.
        dashboard: Validated collector origin for report links, or None.

    Returns:
        At most ``MAX_SUMMARIES`` comments, one per valid issue in the
        answer's order (a repeated issue keeps its first entry).
    """
    issues = payload.get("issues") if isinstance(payload, dict) else payload
    if not isinstance(issues, list):
        return []
    out: list[Rendered] = []
    seen: set[int] = set()
    for entry in issues:
        if len(out) >= MAX_SUMMARIES:
            break
        rendered = render_summary(entry, dashboard)
        if rendered is None or rendered.issue in seen:
            continue
        seen.add(rendered.issue)
        out.append(rendered)
    return out


def main(argv: list[str]) -> int:
    """Writes the summaries of SUMMARIES_FILE to OUT_DIR.

    Args:
        argv: ``[SUMMARIES_FILE, OUT_DIR]`` and optionally ``DASHBOARD_URL``.

    Returns:
        0 when the file was read (possibly with no summaries), 2 on bad usage.
    """
    if len(argv) not in (2, 3):
        print("usage: summary_comment.py SUMMARIES_FILE OUT_DIR [DASHBOARD_URL]", file=sys.stderr)
        return 2
    summaries_path, out_dir = pathlib.Path(argv[0]), pathlib.Path(argv[1])
    dashboard = dashboard_origin(argv[2]) if len(argv) == 3 else None
    if len(argv) == 3 and dashboard is None:
        print("dashboard URL is not a bare https:// origin; rendering without report links", file=sys.stderr)
    try:
        payload = json.loads(summaries_path.read_text(encoding="utf-8", errors="replace"))
    except (json.JSONDecodeError, RecursionError):
        payload = None
    out_dir.mkdir(parents=True, exist_ok=True)
    for n, rendered in enumerate(render_summaries(payload, dashboard)):
        d = out_dir / str(n)
        d.mkdir()
        (d / "issue").write_text(str(rendered.issue), encoding="utf-8")
        (d / "fingerprint").write_text(rendered.fingerprint, encoding="utf-8")
        (d / "body").write_text(rendered.body, encoding="utf-8", errors="replace")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
