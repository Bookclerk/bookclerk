#!/usr/bin/env python3
"""Reports how far the Web API document and the daemon's responses agree.

The daemon's test build layers a check over its router that compares every
response the tests produce with ``crates/bookclerkd/openapi.json``: the
matched route template, the method, the status, the media type and, for a
JSON body, the body against the declared schema (``openapi_contract.rs``).
Each response is one JSON line in ``target/contract-report/<pid>.jsonl``,
one file per test process because cargo-nextest runs each test in its own.
The test job keeps those files with its coverage report, and CI Gate runs
this script over them, which:

1. lists every response the document declares as a ``(method, path,
   status, media)`` pair, with the operation's first tag as its area;
2. reads every hit the test processes recorded and keeps the first hit per
   pair and outcome;
3. counts the declared pairs the tests observed, per area and in all, and
   lists the responses that disagree with the document: a status or media
   type the route does not declare, a body that does not validate, a body
   that is not JSON, and a matched route the document does not describe
   (an HTML page, such as the invite page or the SPA, is served on purpose
   without one and is left out).

The result goes to the step summary as Markdown. In ``report`` mode the
script always exits 0; in ``enforce`` mode a disagreement fails CI Gate,
and so does an observed share below the floor ``[contract] floor`` in
``scripts/coverage_areas.toml`` states (a percentage that only rises, like
the coverage floors; it is not checked when no report file exists, that
is, when the plan did not run the daemon's tests). The parsers are sans-IO
so ``scripts/tests/test_contract_coverage.py`` drives the same code.

Usage: ``contract_coverage.py --reports DIR [--document FILE] [--mode
report|enforce] [--config FILE] [--floor PERCENT] [--summary FILE]
[--format summary|json]`` from the repository root (``--summary`` defaults
to ``$GITHUB_STEP_SUMMARY``).
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tomllib
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path

#: The document the daemon prints, committed beside its sources.
DEFAULT_DOCUMENT = "crates/bookclerkd/openapi.json"
#: The file whose ``[contract] floor`` is the lowest observed share allowed.
DEFAULT_CONFIG = "scripts/coverage_areas.toml"
#: HTTP methods an OpenAPI path item can hold, in JSON key form.
METHODS = ("get", "put", "post", "delete", "options", "head", "patch", "trace")
#: The area of an operation without a tag.
UNTAGGED = "(untagged)"
#: The media of a response without a body, as the layer records it.
NO_MEDIA = "none"
#: How many disagreements of one kind the summary lists.
MAX_LISTED = 40
#: The media of the pages the router serves on purpose without describing
#: them (the invite page and the SPA documents); an undocumented route
#: answering anything else is a finding.
PAGE_MEDIA = "text/html"

#: One declared response: method, route template, status key, media type.
Pair = tuple[str, str, str, str]


@dataclass(frozen=True)
class Hit:
    """One response a test process recorded.

    Attributes:
        method: The HTTP method in lower case.
        path: The route template the router matched.
        status: The response status.
        media: The media type without parameters, or ``none``.
        kind: The outcome kind the layer wrote (``declared``,
            ``undeclared_status``, …).
        detail: The outcome's message or messages, joined, for the lists.
    """

    method: str
    path: str
    status: int
    media: str
    kind: str
    detail: str = ""

    @property
    def pair(self) -> Pair:
        """The declared-response key this hit concerns."""
        return (self.method, self.path, str(self.status), self.media)


@dataclass
class Report:
    """What the summary shows.

    Attributes:
        declared: Every declared pair and its area.
        observed: The declared pairs at least one hit matched.
        disagreements: Hits whose outcome is not ``declared``, one per pair
            and kind, the HTML pages the router serves on purpose left out.
        malformed: Report lines that were not a hit.
        files: How many report files were read.
    """

    declared: dict[Pair, str] = field(default_factory=dict)
    observed: set[Pair] = field(default_factory=set)
    disagreements: list[Hit] = field(default_factory=list)
    malformed: int = 0
    files: int = 0

    def by_area(self) -> dict[str, tuple[int, int]]:
        """Declared and observed counts per area, areas in name order."""
        counts: dict[str, list[int]] = defaultdict(lambda: [0, 0])
        for pair, area in self.declared.items():
            counts[area][0] += 1
            if pair in self.observed:
                counts[area][1] += 1
        return {area: (d, o) for area, (d, o) in sorted(counts.items())}


def media_type(content_type: str) -> str:
    """The media type of a content key without parameters, in lower case."""
    return content_type.split(";", 1)[0].strip().lower() or NO_MEDIA


def declared_pairs(document: dict) -> dict[Pair, str]:
    """Every response the document declares, keyed to its operation's area.

    A response without ``content`` declares the ``none`` media; a
    ``default`` response keeps that key as its status.
    """
    pairs: dict[Pair, str] = {}
    paths = document.get("paths")
    if not isinstance(paths, dict):
        return pairs
    for path, item in paths.items():
        if not isinstance(item, dict):
            continue
        for method in METHODS:
            operation = item.get(method)
            if not isinstance(operation, dict):
                continue
            tags = operation.get("tags")
            area = tags[0] if isinstance(tags, list) and tags and isinstance(tags[0], str) else UNTAGGED
            responses = operation.get("responses")
            if not isinstance(responses, dict):
                continue
            for status, response in responses.items():
                content = response.get("content") if isinstance(response, dict) else None
                medias = [media_type(m) for m in content] if isinstance(content, dict) and content else [NO_MEDIA]
                for media in medias:
                    pairs[(method, path, str(status), media)] = area
    return pairs


def parse_hits(text: str) -> tuple[list[Hit], int]:
    """Parses one report file's lines into hits.

    Returns:
        The hits in order and how many lines were not a hit.
    """
    hits: list[Hit] = []
    malformed = 0
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError:
            malformed += 1
            continue
        hit = _hit(record)
        if hit is None:
            malformed += 1
        else:
            hits.append(hit)
    return hits, malformed


def _hit(record: object) -> Hit | None:
    """One hit from a parsed line, or ``None`` when the line is not one."""
    if not isinstance(record, dict):
        return None
    method, path, status, media, outcome = (
        record.get("method"),
        record.get("path"),
        record.get("status"),
        record.get("media"),
        record.get("outcome"),
    )
    if not (isinstance(method, str) and isinstance(path, str) and isinstance(media, str)):
        return None
    if isinstance(status, bool) or not isinstance(status, int) or not 100 <= status <= 999:
        return None
    if not isinstance(outcome, dict) or not isinstance(outcome.get("kind"), str):
        return None
    detail = ""
    if isinstance(outcome.get("error"), str):
        detail = outcome["error"]
    elif isinstance(outcome.get("errors"), list):
        detail = "; ".join(e for e in outcome["errors"] if isinstance(e, str))
    return Hit(method, path, status, media, outcome["kind"], detail)


def match_status(pair: Pair, declared: dict[Pair, str]) -> Pair | None:
    """The declared pair `pair` observes: its own status, else ``default``."""
    if pair in declared:
        return pair
    fallback = (pair[0], pair[1], "default", pair[3])
    return fallback if fallback in declared else None


def build_report(document: dict, reports: list[str]) -> Report:
    """Compares the hits in `reports` (file contents) with `document`."""
    report = Report(declared=declared_pairs(document), files=len(reports))
    seen: set[tuple[Pair, str]] = set()
    for text in reports:
        hits, malformed = parse_hits(text)
        report.malformed += malformed
        for hit in hits:
            if hit.kind == "declared":
                matched = match_status(hit.pair, report.declared)
                if matched is not None:
                    report.observed.add(matched)
                continue
            if hit.kind == "undocumented" and media_type(hit.media) == PAGE_MEDIA:
                continue
            key = (hit.pair, hit.kind)
            if key not in seen:
                seen.add(key)
                report.disagreements.append(hit)
    report.disagreements.sort(key=lambda h: (h.kind, h.path, h.method, h.status, h.media))
    return report


def percent(observed: int, declared: int) -> str:
    """`observed` out of `declared` as a percentage, ``n/a`` for nothing declared."""
    return f"{100 * observed / declared:.1f}%" if declared else "n/a"


def read_floor(text: str) -> float | None:
    """The ``[contract] floor`` of `text` (``scripts/coverage_areas.toml``).

    Args:
        text: The file's TOML text.

    Returns:
        The floor in percent, or ``None`` when the file states none.

    Raises:
        ValueError: The file is not TOML, or the floor is not a number
            between 0 and 100.
    """
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as err:
        raise ValueError(f"not TOML: {err}") from err
    contract = data.get("contract")
    if contract is None:
        return None
    if not isinstance(contract, dict) or "floor" not in contract:
        raise ValueError("[contract] must hold a floor")
    floor = contract["floor"]
    if isinstance(floor, bool) or not isinstance(floor, (int, float)) or not 0 <= floor <= 100:
        raise ValueError(f"[contract] floor must be a percentage, not {floor!r}")
    return float(floor)


def share(report: Report) -> float | None:
    """The observed share of the declared pairs in percent, ``None`` for nothing declared."""
    return 100 * len(report.observed) / len(report.declared) if report.declared else None


def below_floor(report: Report, floor: float | None) -> bool:
    """True when `report` has report files and its observed share is under `floor`.

    Without a report file the daemon's tests did not run (a selective plan
    without the daemon's package), so the share says nothing and the floor
    is not checked.
    """
    observed = share(report)
    return floor is not None and report.files > 0 and observed is not None and observed < floor


def render(report: Report, mode: str, floor: float | None = None) -> str:
    """The step summary for `report` as Markdown.

    Args:
        report: What the tests observed and where they disagreed.
        mode: ``report`` or ``enforce``, which decides what the findings do.
        floor: The lowest observed share allowed, in percent, if any.
    """
    total = len(report.declared)
    seen = len(report.observed)
    lines = [
        "## Web API contract",
        "",
        f"Declared responses observed by the tests: **{seen} of {total}** ({percent(seen, total)}), "
        f"from {report.files} report file(s).",
        "",
    ]
    if floor is not None:
        if report.files == 0:
            verdict = "not checked: no report file, so the daemon's tests did not run in this plan"
        elif below_floor(report, floor):
            verdict = "**below the floor**; " + ("this fails CI Gate" if mode == "enforce" else "reported only")
        else:
            verdict = "met"
        lines.append(f"Floor: {floor:g}% of the declared responses observed ({verdict}).")
        lines.append("")
    if report.disagreements:
        word = "fail CI Gate" if mode == "enforce" else "are reported only"
        lines.append(f"**{len(report.disagreements)} disagreement(s)** between the document and the daemon; they {word}.")
        lines.append("")
        lines.append("| Kind | Method | Route | Status | Media | Detail |")
        lines.append("| --- | --- | --- | --- | --- | --- |")
        for hit in report.disagreements[:MAX_LISTED]:
            detail = hit.detail.replace("|", "\\|")
            if len(detail) > 160:
                detail = detail[:157] + "…"
            lines.append(
                f"| `{hit.kind}` | `{hit.method.upper()}` | `{hit.path}` | {hit.status} | `{hit.media}` | {detail} |"
            )
        if len(report.disagreements) > MAX_LISTED:
            lines.append(f"| … | | | | | {len(report.disagreements) - MAX_LISTED} more |")
        lines.append("")
    else:
        lines.append("Every response the tests produced agrees with the document.")
        lines.append("")
    lines.append("| Area | Declared | Observed | Share |")
    lines.append("| --- | ---: | ---: | ---: |")
    for area, (declared, observed) in report.by_area().items():
        lines.append(f"| {area} | {declared} | {observed} | {percent(observed, declared)} |")
    if report.malformed:
        lines.append("")
        lines.append(f"{report.malformed} report line(s) were not hits and were skipped.")
    return "\n".join(lines) + "\n"


def as_json(report: Report, floor: float | None = None) -> dict:
    """The report as plain data, for ``--format json``."""
    return {
        "declared": len(report.declared),
        "observed": len(report.observed),
        "share": share(report),
        "floor": floor,
        "below_floor": below_floor(report, floor),
        "areas": {area: {"declared": d, "observed": o} for area, (d, o) in report.by_area().items()},
        "disagreements": [
            {
                "kind": h.kind,
                "method": h.method,
                "path": h.path,
                "status": h.status,
                "media": h.media,
                "detail": h.detail,
            }
            for h in report.disagreements
        ],
        "unobserved": sorted(" ".join(p) for p in report.declared if p not in report.observed),
        "malformed": report.malformed,
        "files": report.files,
    }


def read_reports(directory: Path) -> list[str]:
    """The contents of every ``.jsonl`` file under `directory`, in path order."""
    return [p.read_text(encoding="utf-8", errors="replace") for p in sorted(directory.rglob("*.jsonl"))]


def main(argv: list[str] | None = None) -> int:
    """Runs the report; returns the exit status."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--reports", required=True, type=Path, help="directory holding the .jsonl report files")
    parser.add_argument("--document", default=DEFAULT_DOCUMENT, type=Path, help="the OpenAPI 3.1 document")
    parser.add_argument("--mode", choices=("report", "enforce"), default="report")
    parser.add_argument("--config", default=DEFAULT_CONFIG, type=Path, help="the file whose [contract] floor applies")
    parser.add_argument("--floor", type=float, default=None, help="the lowest observed share allowed, in percent (overrides --config)")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"), help="append the Markdown here")
    parser.add_argument("--format", choices=("summary", "json"), default="summary", help="what to print")
    args = parser.parse_args(argv)

    document = json.loads(args.document.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        print(f"{args.document}: not an OpenAPI document", file=sys.stderr)
        return 2
    floor = args.floor
    if floor is None and args.config.is_file():
        try:
            floor = read_floor(args.config.read_text(encoding="utf-8"))
        except ValueError as err:
            print(f"{args.config}: {err}", file=sys.stderr)
            return 2
    reports = read_reports(args.reports) if args.reports.is_dir() else []
    report = build_report(document, reports)
    markdown = render(report, args.mode, floor)
    if args.format == "json":
        print(json.dumps(as_json(report, floor), indent=2))
    else:
        print(markdown, end="")
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as handle:
            handle.write(markdown)
    if args.mode == "enforce" and (report.disagreements or below_floor(report, floor)):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
