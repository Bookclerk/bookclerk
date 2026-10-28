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
(``.github/workflows/coverage-upload.yml``).
"""

from __future__ import annotations

import argparse
import json
import os
import posixpath
import re
import sys
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
    """

    name: str
    report: str
    root: str
    paths: tuple[str, ...]
    suffixes: tuple[str, ...]
    floor: float
    workspace_check: str | None = None

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
    """

    enforce: bool
    patch_target: float
    areas: tuple[Area, ...]


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
                )
            )
    except (tomllib.TOMLDecodeError, KeyError, TypeError, ValueError, AttributeError) as err:
        if isinstance(err, ConfigError):
            raise
        raise ConfigError(f"{AREAS_FILE}: {err}") from err
    return Config(enforce=mode == "enforce", patch_target=target, areas=tuple(areas))


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
        report is not measured. Only the area's own files are kept.
    """
    workspace = Path.cwd().resolve().as_posix()
    results = []
    for area in config.areas:
        report = reports / area.report
        if not report.is_file():
            results.append(AreaResult(area, measured=False))
            continue
        text = report.read_text(encoding="utf-8", errors="replace")
        hits = {p: h for p, h in parse_lcov(text, area.root, workspace).items() if area.owns(p)}
        partial = area.workspace_check is not None and area.workspace_check not in full
        results.append(AreaResult(area, measured=True, hits=hits, partial=partial))
    return results


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

    A whole area goes to ``<out>/<area>.lcov``. A partial one (a selective
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
        The names of the files written, without ``.lcov``.
    """
    out.mkdir(parents=True, exist_ok=True)
    written = []
    for result in read_reports(config, reports, full):
        if result.measured and result.hits:
            name = f"{result.area.name}-partial" if result.partial else result.area.name
            (out / f"{name}.lcov").write_text(write_lcov(result.hits), encoding="utf-8")
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
    parser.add_argument("--summary", type=Path, default=None, help="Markdown output (default $GITHUB_STEP_SUMMARY)")
    args = parser.parse_args(argv)

    config = parse_config(Path(AREAS_FILE).read_text(encoding="utf-8"))
    if args.export is not None:
        # Without a plan every workspace-gated area counts as partial.
        full = full_workspace_checks(args.plan.read_text(encoding="utf-8")) if args.plan and args.plan.is_file() else frozenset()
        print("Exported: " + (", ".join(export(config, args.reports, args.export, full)) or "nothing"))
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
