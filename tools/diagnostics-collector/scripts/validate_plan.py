#!/usr/bin/env python3
"""Validates a diagnostics triage plan and turns it into issue files.

The plan is model output over untrusted report data, so nothing in it is
trusted: at most five issues, fixed labels, only report IDs from the batch
(an issue needs at least one), bounded sizes, and no @-mentions,
cross-references, links, raw HTML or forged Report IDs sections.

Each body ends in the hidden report block (``report_block``) the dashboard
reads; GitHub does not render it, so people read the run's summary comment,
which links each report's dashboard page, instead.

A plan may also ``attach`` reports to issues already open, by number: only
the issues the collector offered the model (``OPEN_ISSUES_FILE``), only
batch report IDs the collector reads a UUID from (a UUID or a collector
object name), each report under one issue, and at most
``MAX_ATTACHMENTS`` issues. An attached report is taken out of any new
issue, so it is tracked once. Attachments touch no issue body: the caller
sends them to the collector, whose ledger records them.

Usage: ``validate_plan.py PLAN_FILE DATA_FILE OUT_DIR [OPEN_ISSUES_FILE]``.
Each issue becomes ``OUT_DIR/<n>/{title,labels,body}``,
``OUT_DIR/attach.json`` is the collector's ``POST /issues/attach`` body
(``{"links": [{"issue": n, "report_ids": [...]}]}``, empty links when
nothing is attached), and ``OUT_DIR/uncovered.json`` lists the batch's
report IDs that a non-empty plan left out, so the caller can track them in
its fallback issue. Exits non-zero when the reply holds no plan, or a plan
names issues or attachments and none of them is valid, so the caller files
its fallback issue instead.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys
from dataclasses import dataclass

#: Most issues one batch may file.
MAX_ISSUES = 5
#: Most open issues one batch may attach reports to (the collector's
#: ``MAX_ATTACH_ISSUES``).
MAX_ATTACHMENTS = 20
#: Most reports one batch may attach in all (the collector's
#: ``MAX_ATTACH_IDS``); later ones stay with the plan's new issues or go
#: uncovered.
MAX_ATTACHED_REPORTS = 500
#: Longest issue title, in characters, after the prefix is added.
MAX_TITLE_CHARS = 120
#: Longest model-written body, in characters, before the report block.
MAX_BODY_CHARS = 8000
#: Prefix every issue title carries.
TITLE_PREFIX = "[diagnostics] "
#: First line of every body, so filed issues can be found again.
MARKER = "<!-- bookclerk-diagnostics: workers-ai triage -->"
#: A report UUID, the only ID form the summary comment links.
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
#: A collector object name, the other ID form the collector reads a report
#: UUID from (``dashboard/report-ids.ts`` ``reportIdOf``), lowercased.
OBJECT_NAME_RE = re.compile(r"diagnostics/[a-z0-9._+-]{1,64}/([0-9a-f-]{36})\.json")
#: The collector origin a dashboard link may point at (no path, query or userinfo).
DASHBOARD_RE = re.compile(r"https://[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?(?::[0-9]{1,5})?")
#: Longest dashboard origin, so ``<origin>/reports/<uuid>`` (45 more
#: characters) stays a short link in the summary comment.
MAX_DASHBOARD_ORIGIN_CHARS = 255
#: Opening line of the hidden report block (``dashboard/report-ids.ts``
#: ``REPORT_BLOCK_OPEN``); the dashboard reads only the last one.
REPORT_BLOCK_OPEN = "<!-- bookclerk-diagnostics: report ids"
#: Closing line of the hidden report block.
REPORT_BLOCK_CLOSE = "-->"
#: An ID the block may list: the dashboard parser's character set, which has
#: no ``>``, so no ID can end the comment early and render the rest.
BLOCK_ID_RE = re.compile(r"[A-Za-z0-9._:/+-]{1,160}")


class PlanError(Exception):
    """The reply cannot be filed: no plan, or a plan with issues and nothing valid."""


@dataclass(frozen=True)
class Issue:
    """One issue the plan may file.

    Attributes:
        title: Prefixed, single-line, defanged title of at most
            ``MAX_TITLE_CHARS`` characters.
        labels: Comma-separated labels: ``diagnostics``, plus ``bug`` when the
            plan says so.
        body: Defanged body ending in the script-written report block.
        report_ids: The batch report IDs the issue lists, in listed order.
    """

    title: str
    labels: str
    body: str
    report_ids: tuple[str, ...]


@dataclass(frozen=True)
class Attachment:
    """Reports the plan attaches to one issue already open.

    Attributes:
        issue: Number of an issue the collector offered the model.
        report_ids: Distinct batch report UUIDs, in listed order, none of
            them attached to an earlier issue or listed by a new issue.
    """

    issue: int
    report_ids: tuple[str, ...]


@dataclass(frozen=True)
class Plan:
    """A validated reply.

    Attributes:
        issues: New issues to file, at most ``MAX_ISSUES``.
        attachments: Reports to attach to open issues, at most
            ``MAX_ATTACHMENTS`` issues.
    """

    issues: tuple[Issue, ...]
    attachments: tuple[Attachment, ...]


def report_block(report_ids: list[str] | tuple[str, ...]) -> str:
    """Formats the hidden report block that ends every filed issue.

    Args:
        report_ids: IDs to list, each matching ``BLOCK_ID_RE``.

    Returns:
        ``REPORT_BLOCK_OPEN``, one ID per line and ``REPORT_BLOCK_CLOSE``,
        each followed by a newline.

    Raises:
        ValueError: An ID does not match ``BLOCK_ID_RE``; it could close the
            comment early or would not be read back.
    """
    for report_id in report_ids:
        if not BLOCK_ID_RE.fullmatch(report_id):
            raise ValueError(f"report ID cannot be listed: {report_id!r}")
    return REPORT_BLOCK_OPEN + "\n" + "".join(f"{i}\n" for i in report_ids) + REPORT_BLOCK_CLOSE + "\n"


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
    # folding over ASCII only), so the dashboard's TypeScript port of this
    # function (`dashboard/summary.ts`) is the same function, as its
    # cross-language fuzz test checks.
    text = re.sub(r"\](?=[(:])", "]\u200b", text)
    text = re.sub(r"(?i)[a-z0-9+.-]*://\S*|\bwww\.\S*", "[link removed]", text, flags=re.ASCII)
    text = re.sub(r"&(?=#|[A-Za-z][A-Za-z0-9]*;)", "&amp;", text)
    text = re.sub(r"<(?=[A-Za-z/!?])", "&lt;", text)
    text = re.sub(r"@(?=[A-Za-z0-9])", "@\u200b", text)
    text = re.sub(r"(?i)\bGH-(?=\d)", "GH-\u200b", text, flags=re.ASCII)
    return re.sub(r"#(?=\d)", "#\u200b", text, flags=re.ASCII)


def find_plan(text: str) -> dict[str, object]:
    """Finds the first JSON object with an ``issues`` list in a reply.

    Args:
        text: The model's whole reply.

    Returns:
        The plan object, unvalidated beyond ``issues`` being a list.

    Raises:
        PlanError: No such object is in the reply, or its ``issues`` is not
            a list.
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
            if not isinstance(obj["issues"], list):
                raise PlanError("plan issues is not a list")
            return obj
    raise PlanError("reply has no JSON plan")


def _is_int(value: object) -> bool:
    """Whether a decoded JSON value is an integer (``bool`` is not one).

    Args:
        value: Decoded value.

    Returns:
        True for an ``int`` that is not a ``bool``.
    """
    return isinstance(value, int) and not isinstance(value, bool)


def attachable_id(report_id: str) -> bool:
    """Whether the collector's ``POST /issues/attach`` reads a report UUID from an ID.

    Args:
        report_id: A batch report ID.

    Returns:
        True for a report UUID or a collector object name
        ``diagnostics/<version>/<uuid>.json``, in either case.
    """
    lower = report_id.lower()
    if UUID_RE.fullmatch(lower):
        return True
    m = OBJECT_NAME_RE.fullmatch(lower)
    return m is not None and UUID_RE.fullmatch(m.group(1)) is not None


def plan_attachments(planned: object, known: set[str], open_issues: frozenset[int]) -> list[Attachment]:
    """Validates a plan's ``attach`` field.

    Args:
        planned: The plan's ``attach`` value; anything but a list attaches
            nothing.
        known: Report IDs in the batch the model saw.
        open_issues: Issue numbers the collector offered the model.

    Returns:
        One attachment per issue, at most ``MAX_ATTACHMENTS`` issues and
        ``MAX_ATTACHED_REPORTS`` reports, in the order
        the issues were first named: entries naming the same issue are
        merged, entries naming other issues or holding no usable ID are
        dropped, and a report stays under the first issue naming it. Only
        batch IDs the collector reads a UUID from (``attachable_id``) are
        attached, as the batch spells them.
    """
    if not isinstance(planned, list):
        return []
    by_issue: dict[int, list[str]] = {}
    seen: set[str] = set()
    for entry in planned:
        if not isinstance(entry, dict):
            continue
        issue, ids = entry.get("issue"), entry.get("report_ids")
        if not _is_int(issue) or issue not in open_issues or not isinstance(ids, list):
            continue
        if issue not in by_issue and len(by_issue) >= MAX_ATTACHMENTS:
            continue
        for report_id in ids:
            if len(seen) >= MAX_ATTACHED_REPORTS:
                break
            if not isinstance(report_id, str) or report_id in seen or report_id not in known:
                continue
            if not attachable_id(report_id):
                continue
            seen.add(report_id)
            by_issue.setdefault(issue, []).append(report_id)
    return [Attachment(issue=issue, report_ids=tuple(ids)) for issue, ids in by_issue.items() if ids]


def validate_plan(text: str, known: set[str], open_issues: frozenset[int] = frozenset()) -> Plan:
    """Validates a reply into the issues to file and the reports to attach.

    Args:
        text: The model's whole reply.
        known: Report IDs in the batch the model saw; only those that fit
            the report block (``BLOCK_ID_RE``) are listed, and only those
            the collector reads a UUID from (``attachable_id``) attached.
        open_issues: Issue numbers the collector offered the model for
            ``attach``; with none, every attachment is dropped.

    Returns:
        Up to ``MAX_ISSUES`` issues and up to ``MAX_ATTACHMENTS``
        attachments; both empty for ``{"issues": []}``. A report an
        attachment claims is left out of every new issue, and a new issue
        left with no report is dropped.

    Raises:
        PlanError: The reply has no plan, or its plan lists issues or
            attachments and none of the first ``MAX_ISSUES`` issues is valid
            while nothing is attached either.
    """
    plan = find_plan(text)
    planned = plan["issues"]
    if not isinstance(planned, list):
        raise PlanError("plan issues is not a list")
    attachments = plan_attachments(plan.get("attach"), known, open_issues)
    attached = {i for a in attachments for i in a.report_ids}
    out: list[Issue] = []
    for issue in planned[:MAX_ISSUES]:
        if not isinstance(issue, dict):
            continue
        title, body, ids = issue.get("title"), issue.get("body"), issue.get("report_ids")
        if not isinstance(title, str) or not isinstance(body, str) or not isinstance(ids, list):
            continue
        ids = list(
            dict.fromkeys(
                i for i in ids if isinstance(i, str) and i in known and i not in attached and BLOCK_ID_RE.fullmatch(i)
            )
        )
        if not ids:
            continue
        title = " ".join(title.split())
        if not title.startswith(TITLE_PREFIX):
            title = TITLE_PREFIX + title
        # Only this script lists report IDs. defang escapes `<!`, so the model
        # cannot forge a report block; a legacy heading is dropped too.
        body = re.sub(r"(?im)^\s*#+\s*report ids\b.*$", "", body[:MAX_BODY_CHARS])
        out.append(
            Issue(
                title=defang(title)[:MAX_TITLE_CHARS],
                labels="diagnostics,bug" if issue.get("bug") is True else "diagnostics",
                body=MARKER
                + "\n"
                + defang(body)
                + "\n\n"
                + report_block(ids),
                report_ids=tuple(ids),
            )
        )
    # An empty plan is a decision; a plan that names issues or attachments
    # and yields nothing is a bad reply, so let the fallback issue track the
    # batch rather than drop the reports it meant to place.
    attach = plan.get("attach")
    if (planned or (isinstance(attach, list) and attach)) and not out and not attachments:
        raise PlanError("plan has no valid issues or attachments")
    return Plan(issues=tuple(out), attachments=tuple(attachments))


def open_issue_numbers(raw: object) -> frozenset[int]:
    """Reads the issue numbers the collector offered the model.

    Args:
        raw: The decoded ``open_issues`` list from the collector's answer.

    Returns:
        Its positive integers; empty for anything else.
    """
    if not isinstance(raw, list):
        return frozenset()
    return frozenset(n for n in raw if _is_int(n) and n > 0)


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


def uncovered_ids(issues: list[Issue] | tuple[Issue, ...], known: set[str], attachments: tuple[Attachment, ...] | list[Attachment] = ()) -> list[str]:
    """Lists the batch reports a non-empty plan left out.

    An empty plan is the model's decision that nothing needs an issue, so it
    leaves nothing uncovered; a plan that files or attaches anything must
    account for every report, or the rest would never be linked to an issue.

    Args:
        issues: Validated issues.
        known: Report IDs in the batch the model saw.
        attachments: Validated attachments.

    Returns:
        The IDs no issue lists and no attachment claims, sorted; empty when
        both are empty.
    """
    if not issues and not attachments:
        return []
    covered = {i for issue in issues for i in issue.report_ids}
    covered |= {i for a in attachments for i in a.report_ids}
    return sorted(known - covered)


def main(argv: list[str]) -> int:
    """Validates PLAN_FILE against DATA_FILE and writes issue files to OUT_DIR.

    Args:
        argv: ``[PLAN_FILE, DATA_FILE, OUT_DIR]``, optionally followed by
            ``OPEN_ISSUES_FILE`` (a JSON list of the issue numbers the
            collector offered the model).

    Returns:
        0 when the plan is valid (possibly with no issues), 1 otherwise, 2
        for a usage error.
    """
    if len(argv) not in (3, 4):
        print("usage: validate_plan.py PLAN_FILE DATA_FILE OUT_DIR [OPEN_ISSUES_FILE]", file=sys.stderr)
        return 2
    plan_path, data_path, out_dir = (pathlib.Path(a) for a in argv[:3])
    text = plan_path.read_text(encoding="utf-8", errors="replace")
    known = known_ids(json.loads(data_path.read_text(encoding="utf-8")))
    open_issues = frozenset()
    if len(argv) == 4:
        open_issues = open_issue_numbers(json.loads(pathlib.Path(argv[3]).read_text(encoding="utf-8")))
    try:
        plan = validate_plan(text, known, open_issues)
    except PlanError as err:
        print(err, file=sys.stderr)
        return 1
    for n, issue in enumerate(plan.issues):
        d = out_dir / str(n)
        d.mkdir()
        for name, value in (("title", issue.title), ("labels", issue.labels), ("body", issue.body)):
            (d / name).write_text(value, encoding="utf-8", errors="replace")
    links = [{"issue": a.issue, "report_ids": list(a.report_ids)} for a in plan.attachments]
    (out_dir / "attach.json").write_text(json.dumps({"links": links}), encoding="utf-8")
    (out_dir / "uncovered.json").write_text(
        json.dumps(uncovered_ids(plan.issues, known, plan.attachments)), encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
