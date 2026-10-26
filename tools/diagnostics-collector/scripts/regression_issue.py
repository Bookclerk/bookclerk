#!/usr/bin/env python3
"""Turns the regressions in an ingest pull into GitHub issues to file.

The collector's ``GET /report`` marks a pulled report ``regression_of`` a
closed issue when the report shares that issue's error signature and runs
the version the fix shipped in or a later one. This module groups such
reports by the closed issue and the report's ``major.minor`` version line
and writes one issue per group, naming the closed issue, its fix pull
request and the fixed-in version (plain ``#<n>`` references, so GitHub
cross-links them) and listing the report IDs in the hidden report block
every diagnostics issue ends with (``dashboard/report-ids.ts`` reads the
block; the first line is the marker that parser reads the closed issue's
number from).

The pull is the collector's answer, but the versions, platforms and the
closed issue's title in it come from reports anyone can submit and from an
issue anyone with triage rights can edit, so every value is validated or
rewritten before it reaches a public issue: nothing in it can carry a link,
a mention or a reference of its own. The module is sans-IO
(``plan_regressions``); ``scripts/tests/test_regression_issue.py`` replays a
seed corpus and runs seeded mutation fuzzing against an independent oracle.

Usage: ``regression_issue.py REPORTS_FILE OUT_DIR``. Each
issue becomes ``OUT_DIR/<n>/{title,labels,body}`` and
``OUT_DIR/handled.json`` is ``{"handled": [...], "overflow": [...]}``: every
report ID the pull marked as a usable regression (the caller keeps them out
of triage) and the subset that belongs to groups beyond ``MAX_ISSUES`` (the
caller holds those as untriaged). Exits 0 with no issue files when the pull
holds no usable regression.
"""

from __future__ import annotations

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


#: The triage plan validator: one copy of the issue-text sanitizer and the
#: report block.
_vp = _sibling("validate_plan")
defang = _vp.defang
report_block = _vp.report_block

#: Most regression issues one run files; the reports of later groups are
#: handed back as overflow.
MAX_ISSUES = 10
#: Most report IDs one issue lists, the dashboard parser's cap.
MAX_IDS = 500
#: Longest issue title, in characters.
MAX_TITLE_CHARS = 120
#: Longest part of the closed issue's title repeated in the new title.
MAX_ORIGINAL_TITLE_CHARS = 70
#: Prefix every issue title carries.
TITLE_PREFIX = "[diagnostics] "
#: Labels every regression issue gets.
LABELS = "diagnostics,bug,regression"
#: A report ID as the collector names them (object name or UUID).
ID_RE = re.compile(r"[A-Za-z0-9._:-]{1,128}")
#: A version as reports and release tags carry it: ``MAJOR.MINOR[.PATCH]``
#: with an optional prerelease, build or packaging part of plain characters
#: (the collector's ``isPackagingDerivative`` admits ``_`` and ``~`` too).
VERSION_RE = re.compile(r"v?(\d{1,6})\.(\d{1,6})(?:\.\d{1,6})?(?:[-+._~][0-9A-Za-z.+_~-]{1,40})?")
#: A platform name as reports carry it.
OS_RE = re.compile(r"[A-Za-z0-9._-]{1,32}")
#: Largest issue or pull request number taken.
MAX_NUMBER = 999_999_999


@dataclass(frozen=True)
class Issue:
    """One regression issue to file.

    Attributes:
        title: Prefixed, single-line title of at most ``MAX_TITLE_CHARS``
            characters.
        labels: Comma-separated labels (``LABELS``).
        body: Marker line, summary and the hidden report block.
        report_ids: The report IDs the issue lists, in listed order.
        regression_of: The closed issue's number.
        version_line: The ``major.minor`` line the reports run.
    """

    title: str
    labels: str
    body: str
    report_ids: tuple[str, ...]
    regression_of: int
    version_line: str








def _number(value: object) -> int | None:
    """An issue or pull request number, or None for anything else."""
    if isinstance(value, bool) or not isinstance(value, int) or value < 1 or value > MAX_NUMBER:
        return None
    return value


def _version(value: object) -> tuple[str, str] | None:
    """A report's version and its ``major.minor`` line, or None."""
    if not isinstance(value, str):
        return None
    m = VERSION_RE.fullmatch(value.strip())
    if not m:
        return None
    # The pattern admits no link syntax, but ``www.`` and ``GH-1`` would still
    # autolink, so the text is rewritten like every other untrusted value.
    return defang(value.strip()), f"{int(m.group(1))}.{int(m.group(2))}"


def _platform(report: object) -> str:
    """The report's platform name, or ``unknown``."""
    payload = report.get("payload") if isinstance(report, dict) else None
    source = payload if isinstance(payload, dict) else report if isinstance(report, dict) else {}
    value = source.get("os")
    if isinstance(value, str) and OS_RE.fullmatch(value.strip()):
        return defang(value.strip().lower())
    return "unknown"


def _title(value: object) -> str:
    """The closed issue's title, rewritten for public text and capped."""
    text = " ".join(value.split()) if isinstance(value, str) else ""
    if text.startswith(TITLE_PREFIX):
        text = text[len(TITLE_PREFIX) :]
    text = defang(text)
    if len(text) > MAX_ORIGINAL_TITLE_CHARS:
        text = text[: MAX_ORIGINAL_TITLE_CHARS - 1].rstrip() + "…"
    return text


def _join(items: list[str]) -> str:
    """Joins sorted distinct items for prose."""
    return ", ".join(sorted(set(items)))


def _groups(pull: object) -> dict[tuple[int, tuple[int, int]], dict]:
    """Groups a pull's usable regressions by closed issue and version line.

    Args:
        pull: The decoded ``GET /report`` answer (an object with ``reports``)
            or its ``reports`` list.

    Returns:
        For each ``(issue, (major, minor))``, the distinct report IDs in
        pull order (at most ``MAX_IDS``), their versions and platforms, and
        the fixed-in version, fix pull request and title of the first
        report's ``regression_of``. A report whose ID, version or closed
        issue is not valid is left out.
    """
    reports = pull.get("reports") if isinstance(pull, dict) else pull
    groups: dict[tuple[int, tuple[int, int]], dict] = {}
    if not isinstance(reports, list):
        return groups
    for entry in reports:
        if not isinstance(entry, dict):
            continue
        of = entry.get("regression_of")
        if not isinstance(of, dict):
            continue
        issue = _number(of.get("issue"))
        fixed = _version(of.get("fixed_in"))
        report_id = entry.get("report_id")
        report = entry.get("report")
        payload = report.get("payload") if isinstance(report, dict) else None
        version = _version((payload if isinstance(payload, dict) else report if isinstance(report, dict) else {}).get("version"))
        if issue is None or fixed is None or version is None:
            continue
        if not isinstance(report_id, str) or not ID_RE.fullmatch(report_id):
            continue
        major, minor = (int(p) for p in version[1].split("."))
        key = (issue, (major, minor))
        group = groups.setdefault(
            key,
            {"ids": [], "versions": [], "platforms": [], "fixed_in": fixed[0], "pr": _number(of.get("pr_number")), "title": _title(of.get("title"))},
        )
        if report_id in group["ids"]:
            continue
        if len(group["ids"]) >= MAX_IDS:
            continue
        group["ids"].append(report_id)
        group["versions"].append(version[0])
        group["platforms"].append(_platform(report))
    return groups


def plan_regressions(pull: object) -> list[Issue]:
    """Turns a pull's regressions into the issues to file.

    Args:
        pull: The decoded ``GET /report`` answer (an object with ``reports``)
            or its ``reports`` list.

    Returns:
        At most ``MAX_ISSUES`` issues, one per closed issue and version
        line, oldest closed issue and lowest line first; empty when no
        report carries a usable ``regression_of``. The reports of later
        groups are ``overflow_ids``.
    """
    out: list[Issue] = []
    for (issue, line), group in sorted(_groups(pull).items())[:MAX_ISSUES]:
        line_text = f"{line[0]}.{line[1]}"
        original = f": {group['title']}" if group["title"] else ""
        title = f"{TITLE_PREFIX}Regression of #{issue} in {line_text}{original}"[:MAX_TITLE_CHARS]
        fixed_by = f"by #{group['pr']}" if group["pr"] is not None else "by a fix recorded by hand"
        count = len(group["ids"])
        body = (
            f"<!-- bookclerk-diagnostics: regression of #{issue} -->\n"
            f"## Regression of #{issue}\n\n"
            f"{count} report{'s' if count != 1 else ''} from version{'s' if len(set(group['versions'])) != 1 else ''} "
            f"{_join(group['versions'])} on {_join(group['platforms'])} "
            f"{'share' if count != 1 else 'shares'} the error signature of #{issue}, "
            f"which was fixed in {group['fixed_in']} {fixed_by}. "
            f"The fix may not cover the {line_text} line, or the cause came back.\n\n"
            "Filed by the diagnostics ingest run, which never posts report contents; "
            "organization members see each report on the dashboard. "
            "Further reports on this line join this issue while it is open.\n\n"
            + report_block(group["ids"])
        )
        out.append(
            Issue(
                title=title,
                labels=LABELS,
                body=body,
                report_ids=tuple(group["ids"]),
                regression_of=issue,
                version_line=line_text,
            )
        )
    return out


def overflow_ids(pull: object) -> list[str]:
    """The report IDs of the groups beyond ``MAX_ISSUES``, in group order.

    Args:
        pull: The decoded ``GET /report`` answer or its ``reports`` list.

    Returns:
        IDs that are usable regressions but in no planned issue; the caller
        holds them as untriaged rather than sending them to the model.
    """
    return [i for _, group in sorted(_groups(pull).items())[MAX_ISSUES:] for i in group["ids"]]


def main(argv: list[str]) -> int:
    """Writes the regression issues of REPORTS_FILE to OUT_DIR.

    Args:
        argv: ``[REPORTS_FILE, OUT_DIR]``.

    Returns:
        0 when the file was read (possibly with no issues), 2 on bad usage.
    """
    if len(argv) != 2:
        print("usage: regression_issue.py REPORTS_FILE OUT_DIR", file=sys.stderr)
        return 2
    reports_path, out_dir = pathlib.Path(argv[0]), pathlib.Path(argv[1])
    try:
        pull = json.loads(reports_path.read_text(encoding="utf-8", errors="replace"))
    except (json.JSONDecodeError, RecursionError):
        pull = None
    issues = plan_regressions(pull)
    out_dir.mkdir(parents=True, exist_ok=True)
    for n, issue in enumerate(issues):
        d = out_dir / str(n)
        d.mkdir()
        for name, value in (("title", issue.title), ("labels", issue.labels), ("body", issue.body)):
            (d / name).write_text(value, encoding="utf-8", errors="replace")
    overflow = overflow_ids(pull)
    handled = [i for issue in issues for i in issue.report_ids] + overflow
    (out_dir / "handled.json").write_text(json.dumps({"handled": handled, "overflow": overflow}), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
