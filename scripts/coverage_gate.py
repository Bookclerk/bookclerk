#!/usr/bin/env python3
"""Measures test coverage for CI Gate: patch coverage and per-area floors.

Each test job that measures coverage writes one LCOV report per area (the
Rust workspace, each Cloudflare Worker, the Python SDK) to
``<area>/lcov.info`` and uploads it as a ``coverage-*`` artifact. CI Gate
downloads them all into one directory and runs this script, which:

1. reads the areas, their report names, the patch target and the floors
   from ``scripts/coverage_areas.toml``;
2. merges each area's report into executable lines and hit counts, keyed by
   repository-relative path;
3. takes the lines the change adds from the plan job's diff (``git diff
   --unified=0 base...head``, written there because CI Gate checks out one
   commit) and counts how many of the executable ones the tests ran: patch
   coverage;
4. compares each measured area's line coverage with its floor, unless the
   run tested only part of the area (a selective Rust run, read from the
   plan).

A selective pull request run only measures the areas whose tests it ran, so
an area without a report is listed as not measured and its changed lines are
left out of the patch figure. A changed line that is not executable (a
comment, a declaration, a test file the report excludes) counts for nothing.

The result goes to the step summary as Markdown. In ``report`` mode (the
``mode`` key) the script always exits 0 and only warns; in ``enforce`` mode a
patch below target or an area below its floor fails CI Gate. The parsers are
sans-IO so ``scripts/tests/test_coverage_gate.py`` drives the same code.

Usage: ``coverage_gate.py --reports DIR --diff FILE --plan ci-plan.json``
from the repository root (``--summary FILE`` defaults to
``$GITHUB_STEP_SUMMARY``). ``coverage_gate.py --reports DIR --plan
ci-plan.json --export OUT`` instead writes each area's report, read by the
same parser and made repository-relative, to ``OUT/<area>.lcov`` (or
``OUT/<area>-partial.lcov``, see :func:`export`) for the Codecov upload
(``.github/workflows/coverage-upload.yml``); with ``--bundles DIR`` it also
copies the built bundles' files that :func:`select_bundle_files` keeps to
``OUT/bundles/<bundle>/`` for Bundle Analysis.
"""

from __future__ import annotations

import argparse
import enum
import json
import math
import os
import posixpath
import re
import stat
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path

import tomllib

#: The areas file, relative to the repository root.
AREAS_FILE = "scripts/coverage_areas.toml"
#: How many uncovered changed lines the summary lists per file.
MAX_LISTED_LINES = 20
#: A hunk header in ``git diff --unified=0`` output: the new side's first
#: line and, when it is not 1, its line count.
HUNK_RE = re.compile(r"^@@ -[0-9]+(?:,[0-9]+)? \+([0-9]+)(?:,([0-9]+))? @@")
#: The longest hunk the diff parser accepts, in lines; a longer count is
#: not real ``git diff`` output and is skipped rather than expanded.
MAX_HUNK_LINES = 1_000_000

#: A line number or hit count in an LCOV ``DA:`` record: ASCII digits only.
DIGITS_RE = re.compile(r"[0-9]+")
#: Executable lines of one file: line number to hit count.
LineHits = dict[int, int]

#: The largest JUnit report the test results parser reads, in characters;
#: a larger one is skipped whole.
MAX_JUNIT_CHARS = 64 * 1024 * 1024
#: The most test cases kept from one JUnit report.
MAX_TEST_CASES = 200_000
#: The longest test name, class name or suite name kept, in characters.
MAX_NAME_CHARS = 1_000
#: The longest failure text kept per test case, in characters.
MAX_FAILURE_CHARS = 16 * 1024
#: The longest test duration kept, in seconds; longer ones are clamped.
MAX_TEST_SECONDS = 1_000_000.0
#: Characters a kept name or failure text never contains: C0 controls other
#: than tab and newline (carriage return spelled out), DEL to U+009F, and the
#: non-characters XML 1.0 forbids.
UNSAFE_CHARS_RE = re.compile(r"[\x00-\x08\x0b\x0c\r\x0e-\x1f\x7f-\x9f\ufffe\uffff]")
#: Markup a JUnit report never needs and the parser refuses: a document type
#: declaration (and with it every entity declaration).
DTD_RE = re.compile(r"<!DOCTYPE|<!ENTITY", re.IGNORECASE)

#: The bundles CI keeps for Codecov Bundle Analysis, each a directory of the
#: ``bundle-*`` artifacts: the web UI and the three Workers.
BUNDLES = ("ui", "site", "demo", "collector")
#: A kept bundle file's path: up to eight segments of safe characters, none
#: starting with a dot (so no ``..``, no hidden files) or a dash.
BUNDLE_PATH_RE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9._-]{0,127}(?:/[A-Za-z0-9_][A-Za-z0-9._-]{0,127}){0,7}")
#: The asset kinds Bundle Analysis sizes; source maps and notes are left out.
BUNDLE_EXTENSIONS = frozenset(
    {"js", "mjs", "cjs", "css", "html", "wasm", "json", "svg", "png", "jpg", "jpeg", "webp", "avif", "ico", "woff", "woff2", "ttf"}
)
#: Limits on one bundle: a file (Cloudflare's own script limit is 10 MiB
#: compressed), the files kept, and their total size, in bytes.
MAX_BUNDLE_FILE_BYTES = 32 * 1024 * 1024
MAX_BUNDLE_FILES = 5_000
MAX_BUNDLE_BYTES = 128 * 1024 * 1024


@dataclass(frozen=True)
class Area:
    """One part of the repository whose tests produce one coverage report.

    Attributes:
        name: The area's key in the areas file, also shown in the summary.
        report: The area's LCOV report, relative to the reports directory.
        root: The directory relative ``SF:`` paths in the report start from,
            relative to the repository root.
        paths: Repository-relative path prefixes the area owns; a changed
            file belongs to the first area whose prefix it starts with.
        suffixes: File extensions the area measures.
        floor: The lowest line coverage, in percent, the area may report.
        workspace_check: The CI plan check that produces the report when it
            can also run only part of the area's tests (``rust_test`` runs
            only the affected packages on a selective run). The floor is
            checked only when the plan says that check ran the whole
            workspace; ``None`` when the area's tests always run in full.
        test_results: The area's JUnit report, relative to the reports
            directory, for Codecov Test Analytics; ``None`` when its tests
            write none.
    """

    name: str
    report: str
    root: str
    paths: tuple[str, ...]
    suffixes: tuple[str, ...]
    floor: float
    workspace_check: str | None = None
    test_results: str | None = None

    def owns(self, path: str) -> bool:
        """Say whether a repository-relative path belongs to this area.

        Args:
            path: A repository-relative POSIX path.

        Returns:
            Whether the path has one of the area's prefixes and suffixes.
        """
        return path.endswith(self.suffixes) and any(path.startswith(p) for p in self.paths)


@dataclass(frozen=True)
class Config:
    """The parsed areas file.

    Attributes:
        enforce: Whether a miss fails CI Gate (``mode = "enforce"``) or only
            warns (``mode = "report"``).
        patch_target: The lowest patch coverage, in percent, a change may have.
        areas: The areas, in file order.
        generated: Repository-relative paths of generated files. They are
            left out of every area's coverage and of patch coverage, and
            ``.github/codecov.yml`` ignores the same list.
    """

    enforce: bool
    patch_target: float
    areas: tuple[Area, ...]
    generated: frozenset[str] = frozenset()


@dataclass
class AreaResult:
    """What one area measured.

    Attributes:
        area: The area.
        measured: Whether its report was present.
        hits: Executable lines per repository-relative file.
        partial: Whether the run measured only part of the area's tests, so
            its total says nothing about the floor.
    """

    area: Area
    measured: bool
    hits: dict[str, LineHits] = field(default_factory=dict)
    partial: bool = False

    def totals(self) -> tuple[int, int]:
        """Count the area's covered and executable lines.

        Returns:
            Covered lines, then executable lines.
        """
        covered = sum(1 for lines in self.hits.values() for n in lines.values() if n > 0)
        return covered, sum(len(lines) for lines in self.hits.values())


class ConfigError(ValueError):
    """The areas file is not in the expected shape."""


def parse_config(text: str) -> Config:
    """Parse the areas file.

    Args:
        text: The TOML text of ``scripts/coverage_areas.toml``.

    Returns:
        The configuration.

    Raises:
        ConfigError: A key is missing or has the wrong type or value.
    """
    try:
        data = tomllib.loads(text)
        mode = data["mode"]
        if mode not in ("report", "enforce"):
            raise ConfigError(f"mode must be report or enforce, not {mode!r}")
        target = float(data["patch_target"])
        areas = []
        for name, raw in data["areas"].items():
            areas.append(
                Area(
                    name=name,
                    report=str(raw["report"]),
                    root=str(raw.get("root", ".")),
                    paths=tuple(str(p) for p in raw["paths"]),
                    suffixes=tuple(str(s) for s in raw["suffixes"]),
                    floor=float(raw["floor"]),
                    workspace_check=str(raw["workspace_check"]) if "workspace_check" in raw else None,
                    test_results=str(raw["test_results"]) if "test_results" in raw else None,
                )
            )
        generated = frozenset(str(p) for p in data.get("generated", []))
    except (tomllib.TOMLDecodeError, KeyError, TypeError, ValueError, AttributeError) as err:
        if isinstance(err, ConfigError):
            raise
        raise ConfigError(f"{AREAS_FILE}: {err}") from err
    return Config(enforce=mode == "enforce", patch_target=target, areas=tuple(areas), generated=generated)


def normalize(source: str, root: str, workspace: str) -> str | None:
    """Turn an LCOV ``SF:`` path into a repository-relative POSIX path.

    Args:
        source: The path as the report records it.
        root: The area's root, relative to the repository root.
        workspace: The repository root as an absolute POSIX path, for
            reports that record absolute paths.

    Returns:
        The repository-relative path, or ``None`` for a file outside the
        repository (a dependency) or a path that climbs out of it.
    """
    # Collapse leading slashes first: normpath keeps a POSIX `//` prefix.
    path = re.sub(r"^/+", "/", source.replace("\\", "/"))
    if path.startswith("/"):
        path = posixpath.normpath(path)
        prefix = workspace.rstrip("/") + "/"
        if not path.startswith(prefix):
            return None
        path = path[len(prefix) :]
    else:
        path = posixpath.normpath(posixpath.join(root, path))
        if path == ".." or path.startswith("../") or path.startswith("/"):
            return None
    return None if path in ("", ".") else path


def parse_lcov(text: str, root: str, workspace: str) -> dict[str, LineHits]:
    """Read the executable lines and hit counts out of an LCOV report.

    Records for the same file are merged, summing hit counts. A ``DA:``
    line whose line number or hit count is not plain ASCII digits is
    skipped rather than failing the gate.

    Args:
        text: The report.
        root: The area's root (see :func:`normalize`).
        workspace: The repository root (see :func:`normalize`).

    Returns:
        Executable lines per repository-relative file.
    """
    files: dict[str, LineHits] = {}
    current: LineHits | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("SF:"):
            path = normalize(line[3:], root, workspace)
            current = files.setdefault(path, {}) if path else None
        elif line == "end_of_record":
            current = None
        elif line.startswith("DA:") and current is not None:
            fields = line[3:].split(",")
            if len(fields) < 2 or not (DIGITS_RE.fullmatch(fields[0]) and DIGITS_RE.fullmatch(fields[1])):
                continue
            number, hits = int(fields[0]), int(fields[1])
            if number > 0:
                current[number] = current.get(number, 0) + hits
    return {path: lines for path, lines in files.items() if lines}


def parse_diff(text: str) -> dict[str, set[int]]:
    """Read the lines a change adds out of ``git diff --unified=0``.

    Args:
        text: The diff, with ``--no-renames`` not required: a renamed file's
            ``+++`` path is its new name.

    Returns:
        Added line numbers (new side) per repository-relative file; deleted
        files and hunks longer than ``MAX_HUNK_LINES`` are left out.
    """
    added: dict[str, set[int]] = {}
    current: set[int] | None = None
    for line in text.splitlines():
        if line.startswith("+++ "):
            target = line[4:].strip()
            if target == "/dev/null":
                current = None
            else:
                current = added.setdefault(target[2:] if target.startswith("b/") else target, set())
        elif line.startswith("@@") and current is not None:
            hunk = HUNK_RE.match(line)
            if hunk:
                start = int(hunk.group(1))
                count = int(hunk.group(2)) if hunk.group(2) is not None else 1
                if count <= MAX_HUNK_LINES:
                    current.update(range(start, start + count))
    return {path: lines for path, lines in added.items() if lines}


@dataclass(frozen=True)
class FileOutcome:
    """Patch coverage of one changed file.

    Attributes:
        path: The repository-relative path.
        covered: Changed executable lines the tests ran.
        missed: Changed executable lines they did not, in order.
    """

    path: str
    covered: int
    missed: tuple[int, ...]


@dataclass(frozen=True)
class Verdict:
    """The gate's findings.

    Attributes:
        files: Patch coverage of each changed file with executable lines.
        unmeasured: Changed files in areas without a report this run.
        areas: Each area's result.
        patch_percent: Patch coverage, or ``None`` with no executable
            changed lines.
        failures: What misses its target, one sentence each.
    """

    files: tuple[FileOutcome, ...]
    unmeasured: tuple[str, ...]
    areas: tuple[AreaResult, ...]
    patch_percent: float | None
    failures: tuple[str, ...]


def percent(covered: int, total: int) -> float:
    """Express a count as a percentage, 100 for an empty total.

    Args:
        covered: The covered count.
        total: The total count.

    Returns:
        ``covered / total`` in percent.
    """
    return 100.0 * covered / total if total else 100.0


def judge(config: Config, results: list[AreaResult], added: dict[str, set[int]]) -> Verdict:
    """Compute patch coverage and check each area's floor.

    Args:
        config: The areas file.
        results: Each area's measured lines, in the areas file's order.
        added: Lines the change adds, per file.

    Returns:
        The verdict.
    """
    files: list[FileOutcome] = []
    unmeasured: list[str] = []
    for path in sorted(added):
        if path in config.generated:
            continue
        result = next((r for r in results if r.area.owns(path)), None)
        if result is None:
            continue
        if not result.measured:
            unmeasured.append(path)
            continue
        lines = result.hits.get(path, {})
        changed = sorted(n for n in added[path] if n in lines)
        if changed:
            missed = tuple(n for n in changed if lines[n] == 0)
            files.append(FileOutcome(path, len(changed) - len(missed), missed))
    total = sum(f.covered + len(f.missed) for f in files)
    patch = percent(sum(f.covered for f in files), total) if total else None
    failures: list[str] = []
    if patch is not None and patch < config.patch_target:
        failures.append(f"Patch coverage is {patch:.1f}%, below the {config.patch_target:g}% target.")
    for result in results:
        if result.measured and not result.partial:
            covered, lines = result.totals()
            value = percent(covered, lines)
            if value < result.area.floor:
                failures.append(f"{result.area.name} line coverage is {value:.1f}%, below its {result.area.floor:g}% floor.")
    return Verdict(tuple(files), tuple(unmeasured), tuple(results), patch, tuple(failures))


def ranges(numbers: tuple[int, ...]) -> str:
    """Write line numbers as ranges, at most ``MAX_LISTED_LINES`` of them.

    Args:
        numbers: Sorted line numbers.

    Returns:
        Text such as ``3-5, 9``, with ``…`` when lines were left out.
    """
    shown = numbers[:MAX_LISTED_LINES]
    spans: list[str] = []
    start = prev = shown[0] if shown else 0
    for n in shown[1:]:
        if n != prev + 1:
            spans.append(f"{start}" if start == prev else f"{start}-{prev}")
            start = n
        prev = n
    if shown:
        spans.append(f"{start}" if start == prev else f"{start}-{prev}")
    return ", ".join(spans) + (", …" if len(numbers) > MAX_LISTED_LINES else "")


def render(config: Config, verdict: Verdict) -> str:
    """Write the verdict as the step summary's Markdown.

    Args:
        config: The areas file.
        verdict: The verdict.

    Returns:
        The Markdown.
    """
    mode = "enforced" if config.enforce else "report only"
    out = [f"## Coverage ({mode})", ""]
    if verdict.patch_percent is None:
        out.append("Patch coverage: no executable lines changed in a measured area.")
    else:
        lines = sum(f.covered + len(f.missed) for f in verdict.files)
        out.append(
            f"Patch coverage: **{verdict.patch_percent:.1f}%** of {lines} changed executable lines "
            f"(target {config.patch_target:g}%)."
        )
    out += ["", "| Area | Line coverage | Floor |", "| --- | --- | --- |"]
    for result in verdict.areas:
        if result.measured:
            covered, lines = result.totals()
            note = " (affected packages only; floor not checked)" if result.partial else ""
            out.append(
                f"| {result.area.name} | {percent(covered, lines):.1f}% ({covered}/{lines}){note} | {result.area.floor:g}% |"
            )
        else:
            out.append(f"| {result.area.name} | not measured in this run | {result.area.floor:g}% |")
    missed = [f for f in verdict.files if f.missed]
    if missed:
        out += ["", "Changed lines the tests did not run:", ""]
        out += [f"- `{f.path}`: {ranges(f.missed)}" for f in missed]
    if verdict.unmeasured:
        out += ["", "Changed files in areas this run did not measure:", ""]
        out += [f"- `{p}`" for p in verdict.unmeasured]
    if verdict.failures:
        out += [""] + [f"- {f}" for f in verdict.failures]
    return "\n".join(out) + "\n"


def full_workspace_checks(plan: str) -> frozenset[str]:
    """Name the plan's checks that ran the whole workspace.

    Args:
        plan: The text of the run's ``ci-plan.json``.

    Returns:
        Checks whose parameters say ``workspace = true``; empty for a plan
        that is not in the expected shape, so floors are then skipped.
    """
    try:
        checks = json.loads(plan)["execution"]["checks"]
        return frozenset(name for name, check in checks.items() if check.get("params", {}).get("workspace") is True)
    except (json.JSONDecodeError, KeyError, TypeError, AttributeError):
        return frozenset()


def measure(config: Config, reports: Path, diff: Path, plan: Path | None) -> Verdict:
    """Read the downloaded reports, the diff and the plan, and judge them.

    Args:
        config: The areas file.
        reports: The directory the ``coverage-*`` artifacts were downloaded to.
        diff: The plan job's ``git diff --unified=0`` hunk headers.
        plan: The run's ``ci-plan.json``, or ``None`` to treat every
            workspace-gated area as partial.

    Returns:
        The verdict.

    Raises:
        OSError: The diff or the plan could not be read.
    """
    full = full_workspace_checks(plan.read_text(encoding="utf-8")) if plan else frozenset()
    results = read_reports(config, reports, full)
    added = parse_diff(diff.read_text(encoding="utf-8", errors="replace"))
    return judge(config, results, added)


def read_reports(config: Config, reports: Path, full: frozenset[str]) -> list[AreaResult]:
    """Read each area's report from the downloaded artifacts.

    Args:
        config: The areas file.
        reports: The directory the ``coverage-*`` artifacts were downloaded to.
        full: The plan's checks that ran the whole workspace (see
            :func:`full_workspace_checks`); a measured area whose
            ``workspace_check`` is not among them is partial.

    Returns:
        Each area's result, in the areas file's order; an area without a
        report is not measured. Only the area's own files are kept, and
        never a generated one.
    """
    workspace = Path.cwd().resolve().as_posix()
    results = []
    for area in config.areas:
        report = reports / area.report
        if not report.is_file():
            results.append(AreaResult(area, measured=False))
            continue
        text = report.read_text(encoding="utf-8", errors="replace")
        hits = {p: h for p, h in parse_lcov(text, area.root, workspace).items() if area.owns(p) and p not in config.generated}
        partial = area.workspace_check is not None and area.workspace_check not in full
        results.append(AreaResult(area, measured=True, hits=hits, partial=partial))
    return results


class Outcome(enum.Enum):
    """How one test case ended."""

    #: The test ran and passed (a retried test that passed in the end too).
    PASSED = "passed"
    #: An assertion failed (``<failure>``).
    FAILED = "failed"
    #: The test could not run to an assertion (``<error>``).
    ERROR = "error"
    #: The test was not run (``<skipped>``).
    SKIPPED = "skipped"


@dataclass(frozen=True)
class TestCase:
    """One test case read from a JUnit report.

    Attributes:
        classname: The test's suite or module, at most ``MAX_NAME_CHARS``.
        name: The test's name, at most ``MAX_NAME_CHARS``.
        seconds: How long it ran, rounded to milliseconds, between 0 and
            ``MAX_TEST_SECONDS``.
        outcome: How it ended.
        failure: The failure's message and output, at most
            ``MAX_FAILURE_CHARS``; empty unless it failed or errored.
    """

    __test__ = False  # Not a unittest class, despite the name.

    classname: str
    name: str
    seconds: float
    outcome: Outcome
    failure: str = ""


def clean_name(value: str | None) -> str:
    """Keep a JUnit name as one line of safe characters.

    Args:
        value: An attribute value, or ``None`` when it was missing.

    Returns:
        The value with tabs and line breaks turned into spaces, other unsafe
        characters removed, and cut to ``MAX_NAME_CHARS``.
    """
    text = re.sub(r"[\t\r\n]", " ", value or "")
    return UNSAFE_CHARS_RE.sub("", text)[:MAX_NAME_CHARS]


def clean_failure(value: str) -> str:
    """Keep a failure's text as safe characters.

    Args:
        value: The failure's message and output.

    Returns:
        The text with line breaks as ``\\n``, other unsafe characters
        removed, and cut to ``MAX_FAILURE_CHARS``.
    """
    text = value.replace("\r\n", "\n").replace("\r", "\n")
    return UNSAFE_CHARS_RE.sub("", text)[:MAX_FAILURE_CHARS]


def parse_seconds(value: str | None) -> float:
    """Read a JUnit ``time`` attribute.

    Args:
        value: The attribute, or ``None`` when it was missing.

    Returns:
        Seconds rounded to milliseconds and clamped to
        ``0..MAX_TEST_SECONDS``; 0 for a missing or unreadable value.
    """
    try:
        seconds = float(value or "0")
    except ValueError:
        return 0.0
    if not math.isfinite(seconds) or seconds < 0:
        return 0.0
    return round(min(seconds, MAX_TEST_SECONDS), 3)


def parse_junit(text: str) -> list[TestCase]:
    """Read the test cases of a JUnit XML report.

    The report comes from the pull request's own CI run, so it is untrusted:
    a report with a document type declaration, one over
    ``MAX_JUNIT_CHARS`` and one that is not well-formed XML all read as
    empty. Every ``<testcase>`` element counts, however it is nested; a
    ``<failure>`` or ``<error>`` child makes it failed or errored (an error
    wins), otherwise a ``<skipped>`` child makes it skipped. Retries
    reported as ``<flakyFailure>`` or ``<rerunFailure>`` do not change the
    outcome.

    Args:
        text: The report.

    Returns:
        At most ``MAX_TEST_CASES`` test cases, in document order.
    """
    if len(text) > MAX_JUNIT_CHARS or DTD_RE.search(text):
        return []
    try:
        root = ET.fromstring(text)
    except (ET.ParseError, ValueError):
        return []
    cases: list[TestCase] = []
    for element in root.iter("testcase"):
        if len(cases) == MAX_TEST_CASES:
            break
        outcome = Outcome.PASSED
        failure = ""
        for tag, kind in (("error", Outcome.ERROR), ("failure", Outcome.FAILED)):
            found = element.find(tag)
            if found is not None:
                outcome = kind
                parts = [p for p in (found.get("message") or "", found.text or "") if p.strip()]
                failure = clean_failure("\n".join(parts))
                break
        else:
            if element.find("skipped") is not None:
                outcome = Outcome.SKIPPED
        cases.append(
            TestCase(
                classname=clean_name(element.get("classname")),
                name=clean_name(element.get("name")),
                seconds=parse_seconds(element.get("time")),
                outcome=outcome,
                failure=failure,
            )
        )
    return cases


def write_junit(suite: str, cases: list[TestCase]) -> str:
    """Write test cases back out as a minimal JUnit report.

    The Codecov upload sends these instead of the downloaded reports, so
    only what :func:`parse_junit` accepted leaves the run: names, durations,
    outcomes and failure text.

    Args:
        suite: The suite's name (the area).
        cases: The test cases.

    Returns:
        One ``<testsuites>`` document holding one ``<testsuite>``.
    """
    counts = {o: sum(1 for c in cases if c.outcome is o) for o in Outcome}
    root = ET.Element("testsuites")
    element = ET.SubElement(
        root,
        "testsuite",
        name=clean_name(suite),
        tests=str(len(cases)),
        failures=str(counts[Outcome.FAILED]),
        errors=str(counts[Outcome.ERROR]),
        skipped=str(counts[Outcome.SKIPPED]),
    )
    for case in cases:
        test = ET.SubElement(element, "testcase", classname=case.classname, name=case.name, time=f"{case.seconds:.3f}")
        match case.outcome:
            case Outcome.FAILED:
                ET.SubElement(test, "failure").text = case.failure
            case Outcome.ERROR:
                ET.SubElement(test, "error").text = case.failure
            case Outcome.SKIPPED:
                ET.SubElement(test, "skipped")
            case Outcome.PASSED:
                pass
    return '<?xml version="1.0" encoding="UTF-8"?>\n' + ET.tostring(root, encoding="unicode") + "\n"


def write_lcov(hits: dict[str, LineHits]) -> str:
    """Write executable lines back out as a minimal LCOV report.

    The Codecov upload sends these instead of the downloaded reports, so only
    what :func:`parse_lcov` accepted leaves the run: repository-relative
    paths, line numbers and hit counts.

    Args:
        hits: Executable lines per repository-relative file.

    Returns:
        One ``SF:``/``DA:``/``end_of_record`` record per file, in path order.
    """
    out: list[str] = []
    for path in sorted(hits):
        out.append(f"SF:{path}")
        out += [f"DA:{n},{hits[path][n]}" for n in sorted(hits[path])]
        out.append("end_of_record")
    return "".join(f"{line}\n" for line in out)


def export(config: Config, reports: Path, out: Path, full: frozenset[str]) -> list[str]:
    """Write each measured area's report for the Codecov upload.

    A whole area goes to ``<out>/<area>.lcov``, and its test results, read
    by :func:`parse_junit`, to ``<out>/<area>.junit.xml``. A partial one (a selective
    run that tested only the affected packages) goes to
    ``<out>/<area>-partial.lcov`` instead: uploaded under the area's own
    flag it would replace the area's carried-forward figure with the
    selected packages' alone, so it gets a flag of its own that is never
    carried forward.

    Args:
        config: The areas file.
        reports: The directory the ``coverage-*`` artifacts were downloaded to.
        out: The directory to write to; created when missing.
        full: The plan's checks that ran the whole workspace.

    Returns:
        The names of the files written, without their extension.
    """
    out.mkdir(parents=True, exist_ok=True)
    written = []
    for result in read_reports(config, reports, full):
        if result.measured and result.hits:
            name = f"{result.area.name}-partial" if result.partial else result.area.name
            (out / f"{name}.lcov").write_text(write_lcov(result.hits), encoding="utf-8")
            written.append(name)
    for area in config.areas:
        if area.test_results is None:
            continue
        report = reports / area.test_results
        # Checked before reading: a UTF-8 character is at most four bytes.
        if not report.is_file() or report.stat().st_size > 4 * MAX_JUNIT_CHARS:
            continue
        cases = parse_junit(report.read_text(encoding="utf-8", errors="replace"))
        if cases:
            (out / f"{area.name}.junit.xml").write_text(write_junit(area.name, cases), encoding="utf-8")
            written.append(f"{area.name} test results")
    return written


def select_bundle_files(entries: list[tuple[str, int]]) -> list[str]:
    """Pick the files of one downloaded bundle that are sent to Codecov.

    The bundle comes from the pull request's own run, so its file names and
    sizes are untrusted. A file is kept when its path matches
    :data:`BUNDLE_PATH_RE`, its extension is in :data:`BUNDLE_EXTENSIONS`
    and it is at most :data:`MAX_BUNDLE_FILE_BYTES`. Kept files are taken in
    path order until :data:`MAX_BUNDLE_FILES` or :data:`MAX_BUNDLE_BYTES`
    would be passed.

    Args:
        entries: Each regular file as (path relative to the bundle with ``/``
            separators, size in bytes); symbolic links are never listed.

    Returns:
        The kept paths, sorted and without duplicates.
    """
    kept: list[str] = []
    total = 0
    for path, size in sorted(set(entries)):
        if not BUNDLE_PATH_RE.fullmatch(path) or not 0 <= size <= MAX_BUNDLE_FILE_BYTES:
            continue
        _, dot, extension = path.rpartition("/")[2].rpartition(".")
        if not dot or extension.lower() not in BUNDLE_EXTENSIONS:
            continue
        if len(kept) == MAX_BUNDLE_FILES or total + size > MAX_BUNDLE_BYTES:
            break
        kept.append(path)
        total += size
    return kept


def export_bundles(bundles: Path, out: Path) -> list[str]:
    """Copy the files :func:`select_bundle_files` keeps to ``<out>/<bundle>/``.

    Only the bundles named in :data:`BUNDLES` are read; a missing one is
    skipped. Symbolic links, hard-linked files and anything that is not a
    regular file are never followed or copied.

    Args:
        bundles: The directory the ``bundle-*`` artifacts were downloaded to.
        out: The directory to write to; created when missing.

    Returns:
        The names of the bundles written.
    """
    written = []
    for name in BUNDLES:
        root = bundles / name
        if root.is_symlink() or not root.is_dir():
            continue
        entries = []
        for dirpath, dirnames, filenames in os.walk(root):
            # os.walk does not descend into linked directories by default.
            for filename in filenames:
                file = Path(dirpath) / filename
                info = file.lstat()
                # Regular files with one link only: no symbolic or hard links.
                if stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                    entries.append((file.relative_to(root).as_posix(), info.st_size))
            dirnames.sort()
        kept = select_bundle_files(entries)
        for path in kept:
            target = out / name / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((root / path).read_bytes())
        if kept:
            written.append(name)
    return written


def main(argv: list[str] | None = None) -> int:
    """Run the gate.

    Args:
        argv: Command-line arguments; ``sys.argv[1:]`` when ``None``.

    Returns:
        The exit status: 1 when enforcing and something misses its target.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--reports", type=Path, required=True, help="directory holding the downloaded LCOV reports")
    parser.add_argument("--diff", type=Path, default=None, help="the change as `git diff --unified=0` output")
    parser.add_argument(
        "--export", type=Path, default=None, help="write each area's report, repository-relative, to <dir>/<area>.lcov and stop"
    )
    parser.add_argument("--plan", type=Path, default=None, help="the run's ci-plan.json (see full_workspace_checks)")
    parser.add_argument(
        "--bundles", type=Path, default=None, help="with --export: the downloaded bundles, copied to <export>/bundles/<bundle>/"
    )
    parser.add_argument("--summary", type=Path, default=None, help="Markdown output (default $GITHUB_STEP_SUMMARY)")
    args = parser.parse_args(argv)

    config = parse_config(Path(AREAS_FILE).read_text(encoding="utf-8"))
    if args.export is not None:
        # Without a plan every workspace-gated area counts as partial.
        full: frozenset[str] = frozenset()
        if args.plan and args.plan.is_file():
            full = full_workspace_checks(args.plan.read_text(encoding="utf-8"))
        else:
            print(f"::warning title=Coverage upload::No CI plan at {args.plan}; workspace-gated areas upload as partial.")
        print("Exported: " + (", ".join(export(config, args.reports, args.export, full)) or "nothing"))
        if args.bundles is not None:
            bundles = export_bundles(args.bundles, args.export / "bundles")
            print("Bundles: " + (", ".join(bundles) or "none"))
        return 0
    if args.diff is None:
        parser.error("--diff is required unless --export is given")
    level = "error" if config.enforce else "warning"
    try:
        verdict = measure(config, args.reports, args.diff, args.plan)
    except OSError as err:
        # A missing download is a broken run, not a coverage miss: report
        # mode still only warns.
        print(f"::{level} title=Coverage::Could not read the coverage inputs: {err}")
        return 1 if config.enforce else 0

    markdown = render(config, verdict)
    summary = args.summary or (Path(os.environ["GITHUB_STEP_SUMMARY"]) if os.environ.get("GITHUB_STEP_SUMMARY") else None)
    if summary:
        with summary.open("a", encoding="utf-8") as fh:
            fh.write(markdown)
    else:
        sys.stdout.write(markdown)
    for failure in verdict.failures:
        print(f"::{level} title=Coverage::{failure}")
    return 1 if config.enforce and verdict.failures else 0


if __name__ == "__main__":
    sys.exit(main())
