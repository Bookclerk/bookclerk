#!/usr/bin/env python3
"""Scopes and summarizes Stryker mutation runs on the Cloudflare Workers.

The TypeScript side of mutation testing (``docs/contributing/ci.md#mutation-testing``):
``.github/workflows/mutation-workers.yml`` runs Stryker on the lines a pull
request changes in each Worker (``tools/site``, ``tools/demo``,
``tools/diagnostics-collector``) and then this script, which writes the
Markdown summary. Like the Rust job, the trial is report-only and the script
always exits 0.

Two subcommands:

- ``lines --diff FILE --root DIR`` prints Stryker's ``--mutate`` value for
  the Worker at ``DIR``: each changed line range of its TypeScript sources
  as ``path:start-end``, comma-separated and relative to ``DIR``, or nothing
  when no source line changed. Tests, test support, type declarations and
  configuration are not mutated.
- ``report --report FILE --root DIR --name NAME`` reads Stryker's JSON
  report (the mutation-testing-report schema), appends the summary to
  ``$GITHUB_STEP_SUMMARY`` (``--summary FILE`` overrides) and annotates
  each surviving mutant on the diff.

The diff and the report come from the same job, written by git and by
Stryker; parsing is still sans-IO and bounded, and
``scripts/tests/test_stryker_report.py`` fuzzes it against independent
readings.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections.abc import Callable
from pathlib import Path, PurePosixPath

from mutation_report import MAX_LINE, MAX_LISTED, Outcome, Results, _code

#: Largest report read, in bytes; a larger one is reported as unreadable.
MAX_REPORT_BYTES = 64 * 1024 * 1024

#: Most surviving mutants annotated on the diff; the rest are in the summary.
MAX_ANNOTATIONS = 50

#: Stryker's mutant statuses, mapped to the Rust job's outcomes. A status
#: that is not listed (``Ignored``, ``Pending``) is left out.
STATUSES = {
    "Killed": Outcome.CAUGHT,
    "Timeout": Outcome.TIMEOUT,
    "Survived": Outcome.MISSED,
    "NoCoverage": Outcome.MISSED,
    "CompileError": Outcome.UNVIABLE,
    "RuntimeError": Outcome.UNVIABLE,
}

#: Paths ``lines`` passes on: the value goes into a step output and Stryker's
#: comma-separated ``--mutate``, so anything else (a comma, a newline, a
#: quoted name) is left out.
SAFE_PATH = re.compile(r"^[A-Za-z0-9._/-]{1,300}$")

#: Hunk header of a unified diff: the new side's first line and length.
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def is_source(path: str) -> bool:
    """Say whether a Worker-relative path is TypeScript Stryker should mutate.

    Args:
        path: The path relative to the Worker's directory.

    Returns:
        True for a ``.ts`` file that is not a test, test support, a type
        declaration, a configuration file or a dependency.
    """
    parts = PurePosixPath(path).parts
    name = parts[-1] if parts else ""
    if not name.endswith(".ts") or name.endswith((".d.ts", ".config.ts")) or ".test." in name or ".spec." in name:
        return False
    return not any(part in {"node_modules", "test", "tests", "__tests__", "test-support", "coverage"} for part in parts[:-1])


def changed_lines(diff: str, root: str, keep: Callable[[str], bool] = is_source) -> dict[str, list[tuple[int, int]]]:
    """Collect the added lines of a Worker's sources from a unified diff.

    Args:
        diff: The diff, as ``git diff`` writes it.
        root: The Worker's directory, relative to the repository root.
        keep: Says whether a path relative to ``root`` is a source file to
            collect; ``is_source`` by default. The Python SDK's report
            passes its own.

    Returns:
        For each source file under ``root`` (relative to it, matching
        ``SAFE_PATH`` and kept by ``keep``), the ranges of lines the diff
        adds on the new side, merged and sorted.
    """
    prefix = root.rstrip("/") + "/"
    added: dict[str, set[int]] = {}
    current: str | None = None
    line = 0
    remaining = 0
    for text in diff.splitlines():
        if text.startswith("diff --git "):
            current, remaining = None, 0
            continue
        if remaining == 0 and text.startswith("+++ "):
            path = text[4:]
            current = None
            if path.startswith("b/" + prefix):
                relative = path[2 + len(prefix) :]
                if SAFE_PATH.match(relative) and keep(relative) and ".." not in PurePosixPath(relative).parts:
                    current = relative
            continue
        match = HUNK.match(text)
        if match:
            line = int(match.group(1))
            remaining = int(match.group(2)) if match.group(2) is not None else 1
            continue
        if remaining <= 0 or not text:
            continue
        if text[0] == "+":
            if current is not None:
                added.setdefault(current, set()).add(line)
            line += 1
            remaining -= 1
        elif text[0] == " ":
            line += 1
            remaining -= 1
    return {path: _ranges(sorted(lines)) for path, lines in sorted(added.items())}


def _ranges(lines: list[int]) -> list[tuple[int, int]]:
    """Merge sorted line numbers into inclusive ``(start, end)`` runs."""
    runs: list[tuple[int, int]] = []
    for line in lines:
        if runs and runs[-1][1] + 1 == line:
            runs[-1] = (runs[-1][0], line)
        else:
            runs.append((line, line))
    return runs


def mutate_patterns(lines: dict[str, list[tuple[int, int]]]) -> str:
    """Write Stryker's ``--mutate`` value for the changed lines.

    Args:
        lines: The ranges per file, from ``changed_lines``.

    Returns:
        ``path:start-end`` entries, comma-separated; empty when there are
        none. Only paths matching ``SAFE_PATH`` are written.
    """
    return ",".join(f"{path}:{start}-{end}" for path, runs in lines.items() if SAFE_PATH.match(path) for start, end in runs)


def _clean(text: object) -> str:
    """Make a report field printable and bounded, whatever its type."""
    value = text if isinstance(text, str) else ""
    return "".join(c if c.isprintable() else "?" for c in value)[:MAX_LINE]


def _position(mutant: dict[str, object]) -> tuple[int, int]:
    """Return a mutant's start line and column, or ``(0, 0)`` without one."""
    location = mutant.get("location")
    start = location.get("start") if isinstance(location, dict) else None
    if not isinstance(start, dict):
        return 0, 0
    line, column = start.get("line"), start.get("column")
    if type(line) is int and type(column) is int and 0 < line < 10**7 and 0 <= column < 10**7:
        return line, column
    return 0, 0


def parse_report(text: str, root: str) -> tuple[Results, list[tuple[str, int, str]]] | None:
    """Read Stryker's JSON report.

    Args:
        text: The report.
        root: The Worker's directory, prefixed to each file so descriptions
            and annotations are repository-relative.

    Returns:
        The mutants by outcome, each described as ``path:line:col:
        mutator → replacement``, and ``(path, line, description)`` for each
        surviving one; ``None`` when the report is not a Stryker report.
    """
    try:
        report = json.loads(text)
    except ValueError:
        return None
    files = report.get("files") if isinstance(report, dict) else None
    if not isinstance(files, dict):
        return None
    results = Results()
    survivors: list[tuple[str, int, str]] = []
    prefix = root.rstrip("/") + "/"
    for name, entry in files.items():
        mutants = entry.get("mutants") if isinstance(entry, dict) else None
        if not isinstance(name, str) or not isinstance(mutants, list):
            continue
        path = _clean(prefix + name)
        for mutant in mutants:
            if not isinstance(mutant, dict):
                continue
            status = mutant.get("status")
            outcome = STATUSES.get(status) if isinstance(status, str) else None
            if outcome is None:
                continue
            line, column = _position(mutant)
            replacement = _clean(mutant.get("replacement")) or "(removed)"
            description = _clean(f"{path}:{line}:{column}: {_clean(mutant.get('mutatorName'))} → {replacement}")
            results.mutants[outcome].append(description)
            if outcome is Outcome.MISSED and line:
                survivors.append((path, line, description))
    return results, survivors


def render(name: str, parsed: tuple[Results, list[tuple[str, int, str]]] | None, *, mutated: bool, stopped: bool = False) -> str:
    """Write the Markdown summary of one Worker's run.

    Args:
        name: The Worker's name, for the heading.
        parsed: The parsed report, or ``None`` when Stryker left none.
        mutated: Whether the pull request changed any of the Worker's
            source lines, so Stryker ran.
        stopped: Whether the workflow stopped Stryker at its time limit, so
            the report holds only the mutants tested before then.

    Returns:
        The summary, ending with a newline.
    """
    lines = [f"## Mutation testing ({name} Worker)", ""]
    if not mutated:
        lines.append("No mutants to test: the change touches no source line of this Worker.")
    elif parsed is None:
        lines.append("Stryker did not finish; check the job log.")
    else:
        results = parsed[0]
        if stopped:
            lines += ["Stryker reached the time limit, so only the mutants tested before it are counted.", ""]
        score = results.score()
        if score is None and results.count(Outcome.UNVIABLE):
            lines.append("Stryker made mutants of the changed lines, but none could be tested (all unviable).")
        elif score is None:
            lines.append("Stryker found no mutant on the changed lines.")
        else:
            missed = results.count(Outcome.MISSED)
            lines.append(f"The tests caught **{score:.0f}%** of the {results.tested} mutants of the changed lines ({missed} missed).")
        counts = (results.count(o) for o in (Outcome.CAUGHT, Outcome.MISSED, Outcome.TIMEOUT, Outcome.UNVIABLE))
        lines += [
            "",
            "| Caught | Missed | Timed out | Unviable |",
            "| ---: | ---: | ---: | ---: |",
            "| " + " | ".join(str(c) for c in counts) + " |",
        ]
        missed_list = results.mutants[Outcome.MISSED]
        if missed_list:
            lines += ["", "Missed mutants: the tests still pass with each of these changes.", ""]
            lines += [f"- {_code(m)}" for m in missed_list[:MAX_LISTED]]
            if len(missed_list) > MAX_LISTED:
                lines.append(f"- … and {len(missed_list) - MAX_LISTED} more (see the `mutants-{name}` artifact).")
    lines += ["", "Report only: missed mutants do not fail the pull request yet."]
    return "\n".join(lines) + "\n"


def _escape(value: str, *, property_value: bool) -> str:
    """Escape text for a GitHub workflow command, per its documented rules."""
    value = value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    if property_value:
        value = value.replace(":", "%3A").replace(",", "%2C")
    return value


def annotations(survivors: list[tuple[str, int, str]]) -> str:
    """Write a warning annotation for each surviving mutant.

    Args:
        survivors: ``(path, line, description)`` per surviving mutant.

    Returns:
        One ``::warning`` workflow command per mutant, at most
        ``MAX_ANNOTATIONS``, each ending with a newline.
    """
    out = []
    for path, line, description in survivors[:MAX_ANNOTATIONS]:
        file = _escape(path, property_value=True)
        message = _escape(f"Missed mutant: {description}", property_value=False)
        out.append(f"::warning file={file},line={line},title=Mutation testing::{message}\n")
    return "".join(out)


def lines_main(argv: list[str]) -> int:
    """Print Stryker's ``--mutate`` value for one Worker's changed lines."""
    parser = argparse.ArgumentParser(prog="stryker_report.py lines")
    parser.add_argument("--diff", type=Path, required=True, help="the pull request's diff")
    parser.add_argument("--root", required=True, help="the Worker's directory, relative to the repository root")
    args = parser.parse_args(argv)
    patterns = mutate_patterns(changed_lines(args.diff.read_text(encoding="utf-8", errors="replace"), args.root))
    sys.stdout.write(patterns + "\n" if patterns else "")
    return 0


def report_main(argv: list[str]) -> int:
    """Write one Worker's summary and annotations."""
    parser = argparse.ArgumentParser(prog="stryker_report.py report")
    parser.add_argument("--report", type=Path, required=True, help="Stryker's JSON report")
    parser.add_argument("--root", required=True, help="the Worker's directory, relative to the repository root")
    parser.add_argument("--name", required=True, help="the Worker's name, for the heading and artifact")
    parser.add_argument("--not-mutated", action="store_true", help="the change touched no source line, so Stryker did not run")
    parser.add_argument("--stopped", action="store_true", help="the workflow stopped Stryker at its time limit")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""), help="file to append the summary to (none when empty)")
    args = parser.parse_args(argv)
    parsed = None
    if not args.not_mutated and args.report.is_file() and args.report.stat().st_size <= MAX_REPORT_BYTES:
        parsed = parse_report(args.report.read_text(encoding="utf-8", errors="replace"), args.root)
    markdown = render(_clean(args.name), parsed, mutated=not args.not_mutated, stopped=args.stopped)
    sys.stdout.write(markdown)
    if parsed is not None:
        sys.stdout.write(annotations(parsed[1]))
    if args.summary:
        with Path(args.summary).open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    return 0


def main(argv: list[str] | None = None) -> int:
    """Run ``lines`` or ``report``.

    Args:
        argv: Command-line arguments; ``sys.argv`` when ``None``.

    Returns:
        0: the trial never fails the workflow.
    """
    argv = sys.argv[1:] if argv is None else argv
    commands = {"lines": lines_main, "report": report_main}
    if not argv or argv[0] not in commands:
        sys.stderr.write("usage: stryker_report.py {lines,report} ...\n")
        return 2
    return commands[argv[0]](argv[1:])


if __name__ == "__main__":
    sys.exit(main())
