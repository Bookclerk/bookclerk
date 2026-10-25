#!/usr/bin/env python3
"""Validates a diagnostics triage plan and turns it into issue files.

The plan is model output over untrusted report data, so nothing in it is
trusted: at most five issues, fixed labels, only report IDs from the batch
(an issue needs at least one), bounded sizes, and no @-mentions,
cross-references, links, raw HTML or forged Report IDs sections.

Usage: ``validate_plan.py PLAN_FILE DATA_FILE OUT_DIR [DASHBOARD_URL]``.
Each issue becomes ``OUT_DIR/<n>/{title,labels,body}``, and
``OUT_DIR/uncovered.json`` lists the batch's report IDs that a non-empty plan
left out, so the caller can track them in its fallback issue. With
``DASHBOARD_URL`` (the collector's ``https://`` origin), each listed report
UUID links to its dashboard page; only organization members can open it.
Exits non-zero when the reply holds no plan, or a non-empty plan holds no
valid issue, so the caller files its fallback issue instead.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys
from dataclasses import dataclass

#: Most issues one batch may file.
MAX_ISSUES = 5
#: Longest issue title, in characters, after the prefix is added.
MAX_TITLE_CHARS = 120
#: Longest model-written body, in characters, before the Report IDs section.
MAX_BODY_CHARS = 8000
#: Prefix every issue title carries.
TITLE_PREFIX = "[diagnostics] "
#: First line of every body, so filed issues can be found again.
MARKER = "<!-- bookclerk-diagnostics: workers-ai triage -->"
#: A report UUID, the only ID form that gets a dashboard link.
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
#: The collector origin a dashboard link may point at (no path, query or userinfo).
DASHBOARD_RE = re.compile(r"https://[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?(?::[0-9]{1,5})?")
#: Longest dashboard origin, so ``<origin>/reports/<uuid>`` (45 more
#: characters) fits the 300-character link the dashboard's Report IDs parser
#: (``dashboard/report-ids.js``) reads.
MAX_DASHBOARD_ORIGIN_CHARS = 255


class PlanError(Exception):
    """The reply cannot be filed: no plan, or a non-empty plan with no valid issue."""


@dataclass(frozen=True)
class Issue:
    """One issue the plan may file.

    Attributes:
        title: Prefixed, single-line, defanged title of at most
            ``MAX_TITLE_CHARS`` characters.
        labels: Comma-separated labels: ``diagnostics``, plus ``bug`` when the
            plan says so.
        body: Defanged body ending in the script-written Report IDs section.
        report_ids: The batch report IDs the issue lists, in listed order.
    """

    title: str
    labels: str
    body: str
    report_ids: tuple[str, ...]


def id_line(report_id: str, dashboard: str | None) -> str:
    """Formats one line of the Report IDs section.

    Args:
        report_id: A report ID from the batch.
        dashboard: Validated collector origin, or None for no link.

    Returns:
        ``- `<id>```, followed by `` · [dashboard](<origin>/reports/<id>)``
        when there is a dashboard and the ID is a report UUID, and a newline.
    """
    line = f"- `{report_id}`"
    if dashboard and UUID_RE.fullmatch(report_id):
        line += f" \u00b7 [dashboard]({dashboard}/reports/{report_id})"
    return line + "\n"


def dashboard_origin(raw: str | None) -> str | None:
    """Validates the collector origin dashboard links point at.

    Args:
        raw: The command-line value, possibly with a trailing slash.

    Returns:
        The lowercase origin without a trailing slash, or None when it is
        missing or not a bare ``https://`` origin.
    """
    if not raw:
        return None
    origin = raw.strip(" \t\n\r\x0b\x0c").rstrip("/").lower()
    if len(origin) > MAX_DASHBOARD_ORIGIN_CHARS or not DASHBOARD_RE.fullmatch(origin):
        return None
    return origin


def defang(text: str) -> str:
    """Strips links and breaks mentions and references in public text.

    A report can steer the model, so its text may not carry phishing or
    tracking links, ping people, or cross-reference other issues.

    Args:
        text: Model-written title or body.

    Returns:
        The text with Markdown link, image and reference-definition syntax
        broken, every ``scheme://`` or ``www.`` URL removed, raw HTML tags
        and character references escaped, and
        ``@user`` / ``@org/team`` mentions and ``#123`` / ``owner/repo#123``
        / ``GH-123`` references broken by a zero-width space.
    """
    # `](` and `]:` are what make inline links and reference definitions.
    # ASCII matching throughout (`re.ASCII`: `\b`, `\d`, `\S` and case
    # folding over ASCII only), so the dashboard's JavaScript port of this
    # function (`dashboard/summary.js`) is the same function, as its
    # cross-language fuzz test checks.
    text = re.sub(r"\](?=[(:])", "]\u200b", text)
    text = re.sub(r"(?i)[a-z0-9+.-]*://\S*|\bwww\.\S*", "[link removed]", text, flags=re.ASCII)
    text = re.sub(r"&(?=#|[A-Za-z][A-Za-z0-9]*;)", "&amp;", text)
    text = re.sub(r"<(?=[A-Za-z/!?])", "&lt;", text)
    text = re.sub(r"@(?=[A-Za-z0-9])", "@\u200b", text)
    text = re.sub(r"(?i)\bGH-(?=\d)", "GH-\u200b", text, flags=re.ASCII)
    return re.sub(r"#(?=\d)", "#\u200b", text, flags=re.ASCII)


def find_plan(text: str) -> list[object]:
    """Finds the first JSON object with an ``issues`` list in a reply.

    Args:
        text: The model's whole reply.

    Returns:
        The plan's ``issues`` list, unvalidated.

    Raises:
        PlanError: No such object is in the reply.
    """
    decoder = json.JSONDecoder()
    for i, ch in enumerate(text):
        if ch != "{":
            continue
        try:
            obj, _ = decoder.raw_decode(text, i)
        except json.JSONDecodeError:
            continue
        except RecursionError:
            raise PlanError("reply nests too deeply") from None
        if isinstance(obj, dict) and "issues" in obj:
            issues = obj["issues"]
            if not isinstance(issues, list):
                raise PlanError("plan issues is not a list")
            return issues
    raise PlanError("reply has no JSON plan")


def plan_issues(text: str, known: set[str], dashboard: str | None = None) -> list[Issue]:
    """Validates a reply into the issues to file.

    Args:
        text: The model's whole reply.
        known: Report IDs in the batch the model saw.
        dashboard: Validated collector origin for report links, or None.

    Returns:
        Up to ``MAX_ISSUES`` issues; empty when the plan is ``{"issues": []}``.

    Raises:
        PlanError: The reply has no plan, or its plan lists issues and none
            of the first ``MAX_ISSUES`` is valid.
    """
    planned = find_plan(text)
    out: list[Issue] = []
    for issue in planned[:MAX_ISSUES]:
        if not isinstance(issue, dict):
            continue
        title, body, ids = issue.get("title"), issue.get("body"), issue.get("report_ids")
        if not isinstance(title, str) or not isinstance(body, str) or not isinstance(ids, list):
            continue
        ids = list(dict.fromkeys(i for i in ids if isinstance(i, str) and i in known))
        if not ids:
            continue
        title = " ".join(title.split())
        if not title.startswith(TITLE_PREFIX):
            title = TITLE_PREFIX + title
        # Only this script writes the Report IDs section.
        body = re.sub(r"(?im)^\s*#+\s*report ids\b.*$", "", body[:MAX_BODY_CHARS])
        out.append(
            Issue(
                title=defang(title)[:MAX_TITLE_CHARS],
                labels="diagnostics,bug" if issue.get("bug") is True else "diagnostics",
                body=MARKER
                + "\n"
                + defang(body)
                + "\n\n### Report IDs\n"
                + "".join(id_line(i, dashboard) for i in ids),
                report_ids=tuple(ids),
            )
        )
    # An empty plan is a decision; a non-empty one with nothing valid is a bad
    # reply, so let the fallback issue track the batch.
    if planned and not out:
        raise PlanError("plan has no valid issues")
    return out


def known_ids(batch: object) -> set[str]:
    """Collects the report IDs of a sanitized batch.

    Args:
        batch: The decoded batch the model saw (a list of report objects).

    Returns:
        Every string ``report_id`` in the batch.
    """
    if not isinstance(batch, list):
        return set()
    return {
        r["report_id"]
        for r in batch
        if isinstance(r, dict) and isinstance(r.get("report_id"), str)
    }


def uncovered_ids(issues: list[Issue], known: set[str]) -> list[str]:
    """Lists the batch reports a non-empty plan left out.

    An empty plan is the model's decision that nothing needs an issue, so it
    leaves nothing uncovered; a plan that files some issues must account for
    every report, or the rest would never be linked to an issue.

    Args:
        issues: Validated issues.
        known: Report IDs in the batch the model saw.

    Returns:
        The IDs no issue lists, sorted; empty when ``issues`` is empty.
    """
    if not issues:
        return []
    covered = {i for issue in issues for i in issue.report_ids}
    return sorted(known - covered)


def main(argv: list[str]) -> int:
    """Validates PLAN_FILE against DATA_FILE and writes issue files to OUT_DIR.

    Args:
        argv: ``[PLAN_FILE, DATA_FILE, OUT_DIR]`` and optionally
            ``DASHBOARD_URL``.

    Returns:
        0 when the plan is valid (possibly with no issues), 1 otherwise.
    """
    if len(argv) not in (3, 4):
        print("usage: validate_plan.py PLAN_FILE DATA_FILE OUT_DIR [DASHBOARD_URL]", file=sys.stderr)
        return 2
    plan_path, data_path, out_dir = (pathlib.Path(a) for a in argv[:3])
    dashboard = dashboard_origin(argv[3]) if len(argv) == 4 else None
    if len(argv) == 4 and dashboard is None:
        print("dashboard URL is not a bare https:// origin; filing without report links", file=sys.stderr)
    text = plan_path.read_text(encoding="utf-8", errors="replace")
    known = known_ids(json.loads(data_path.read_text(encoding="utf-8")))
    try:
        issues = plan_issues(text, known, dashboard)
    except PlanError as err:
        print(err, file=sys.stderr)
        return 1
    for n, issue in enumerate(issues):
        d = out_dir / str(n)
        d.mkdir()
        for name, value in (("title", issue.title), ("labels", issue.labels), ("body", issue.body)):
            (d / name).write_text(value, encoding="utf-8", errors="replace")
    (out_dir / "uncovered.json").write_text(json.dumps(uncovered_ids(issues, known)), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
