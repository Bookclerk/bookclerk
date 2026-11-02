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

#: Hunk header of a unified diff: the old side's length, the new side's
#: first line and length.
HUNK = re.compile(r"^@@ -\d+(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")

#: Most lines sampled across the files a run widens to. Stryker took about
#: five seconds a mutant on the site Worker, with two or three mutants a
#: sampled line, so this keeps the second pass to about six minutes.
WIDEN_BUDGET = 30

#: A line ``widen`` never samples on its own account: blank, a comment, or
#: the start of an import or export list.
SKIP_LINE = re.compile(r"^\s*(?:$|//|/\*|\*|import\b|export\s+(?:type\s+)?\{|export\s+\*)")

#: The start of a type-only declaration, whose body Stryker does not mutate.
TYPE_START = re.compile(r"^\s*(?:export\s+)?(?:declare\s+)?(?:interface|type)\b")

#: Something Stryker can mutate on a line: a literal, an operator, a call, a
#: condition or a statement keyword. A line of punctuation or bare names
#: (a closing brace, an import specifier) has none.
MUTABLE = re.compile(r"[\"'`(\[?!<>=&|+\-*/%]|\b(?:return|if|throw|new|await|true|false|null|undefined)\b|[0-9]")


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
    """Collect the changed lines of a Worker's (or the Python SDK's) sources from a unified diff.

    Args:
        diff: The diff, as ``git diff`` writes it.
        root: The Worker's directory, relative to the repository root.
        keep: Says whether a path relative to ``root`` is a source file to
            collect; ``is_source`` by default. The Python SDK's report
            passes its own.

    Returns:
        For each source file under ``root`` (relative to it, matching
        ``SAFE_PATH`` and kept by ``keep``), the ranges of lines the diff adds on the new side,
        plus, for each run of deleted lines that no added line replaces,
        the line just before it (the first line when the run starts the
        file), merged and sorted: a deleted guard then counts as a change to
        the code around it.
    """
    prefix = root.rstrip("/") + "/"
    added: dict[str, set[int]] = {}
    current: str | None = None
    line = 0
    old_remaining = remaining = 0
    # The line before a run of deletions, kept unless an addition follows.
    pending: int | None = None

    def settle() -> None:
        nonlocal pending
        if pending is not None and current is not None:
            added.setdefault(current, set()).add(pending)
        pending = None

    for text in diff.splitlines():
        if text.startswith("diff --git "):
            settle()
            current, old_remaining, remaining = None, 0, 0
            continue
        if old_remaining == 0 and remaining == 0 and text.startswith("+++ "):
            path = text[4:]
            current = None
            if path.startswith("b/" + prefix):
                relative = path[2 + len(prefix) :]
                if SAFE_PATH.match(relative) and keep(relative) and ".." not in PurePosixPath(relative).parts:
                    current = relative
            continue
        match = HUNK.match(text)
        if match:
            settle()
            old_remaining = int(match.group(1)) if match.group(1) is not None else 1
            line = int(match.group(2))
            remaining = int(match.group(3)) if match.group(3) is not None else 1
            if remaining == 0:
                # A hunk that only deletes names the new-side line before
                # it (0 at the top of the file), not the line after.
                line += 1
            continue
        if (old_remaining <= 0 and remaining <= 0) or not text:
            continue
        if text[0] == "+" and remaining > 0:
            pending = None
            if current is not None:
                added.setdefault(current, set()).add(line)
            line += 1
            remaining -= 1
        elif text[0] == "-" and old_remaining > 0:
            if pending is None:
                pending = max(line - 1, 1)
            old_remaining -= 1
        elif text[0] == " ":
            settle()
            line += 1
            old_remaining -= 1
            remaining -= 1
    settle()
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


def parse_mutate(value: str) -> dict[str, list[tuple[int, int]]]:
    """Read a ``--mutate`` value ``mutate_patterns`` wrote back into ranges.

    Args:
        value: ``path:start-end`` entries, comma-separated.

    Returns:
        The ranges per path, in the order given; entries of another shape
        are left out.
    """
    ranges: dict[str, list[tuple[int, int]]] = {}
    for entry in value.strip().split(",")[:10000]:
        match = re.match(r"^([A-Za-z0-9._/-]{1,300}):([0-9]{1,7})-([0-9]{1,7})$", entry)
        if match and SAFE_PATH.match(match.group(1)):
            start, end = int(match.group(2)), int(match.group(3))
            if 0 < start <= end:
                ranges.setdefault(match.group(1), []).append((start, end))
    return ranges


def _span(mutant: dict[str, object]) -> range:
    """Return the lines a mutant covers, start to end (empty without a location)."""
    start, _ = _position(mutant)
    location = mutant.get("location")
    end = location.get("end") if isinstance(location, dict) else None
    last = end.get("line") if isinstance(end, dict) else None
    if not start:
        return range(0)
    if type(last) is not int or not start <= last < start + 10000:
        last = start
    return range(start, last + 1)


def mutant_lines(text: str) -> dict[str, set[int]] | None:
    """Read the lines every mutant in a Stryker report covers, per file.

    Args:
        text: The report's JSON.

    Returns:
        The lines per Worker-relative file, from each mutant's start to its
        end, whatever its status, or ``None`` when the text is not a Stryker
        report.
    """
    try:
        report = json.loads(text)
    except (ValueError, RecursionError):
        return None
    files = report.get("files") if isinstance(report, dict) else None
    if not isinstance(files, dict):
        return None
    lines: dict[str, set[int]] = {}
    for name, entry in files.items():
        mutants = entry.get("mutants") if isinstance(entry, dict) else None
        if isinstance(name, str) and isinstance(mutants, list):
            lines[name] = {line for m in mutants if isinstance(m, dict) for line in _span(m)}
    return lines


def sample_lines(source: str) -> list[int]:
    """List the lines of a TypeScript file that likely hold a mutant.

    Stryker mutates expressions, statements and literals. This leaves out
    what it never mutates: blank lines, comments, import and export lists
    (to their ``from`` line), interface and type declarations (to their
    closing brace or semicolon), and lines with nothing mutable on them.

    Args:
        source: The file's text.

    Returns:
        The 1-based lines worth sampling, in order.
    """
    out: list[int] = []
    depth = 0
    in_list = False
    for number, text in enumerate(source.splitlines(), start=1):
        if depth > 0:
            depth += text.count("{") - text.count("}")
            depth = max(depth, 0)
            continue
        if in_list:
            in_list = not re.search(r"\bfrom\b|^\s*\}\s*;?\s*$", text)
            continue
        if TYPE_START.match(text):
            opened = text.count("{") - text.count("}")
            depth = opened if opened > 0 else 0
            continue
        if SKIP_LINE.match(text):
            in_list = bool(re.match(r"^\s*(?:import|export)\b", text)) and "{" in text and "}" not in text
            continue
        if MUTABLE.search(text):
            out.append(number)
    return out


def widen(
    ranges: dict[str, list[tuple[int, int]]],
    mutated: dict[str, set[int]],
    sources: dict[str, str],
    seed: int,
    budget: int = WIDEN_BUDGET,
) -> dict[str, list[int]]:
    """Choose lines to mutate in files whose changed lines gave no mutant.

    Stryker mutates expressions, statements and literals, so a change to a
    type, an import or an export gives no mutant; the tests of the code
    beside it are then checked on a sample of the file's other lines, as
    the Rust job does for data changes. Every line can bring in several
    mutants at a few seconds each, so at most ``budget`` lines are chosen,
    spread across the files: each file gets one first, then the rest are
    evenly spaced over the remaining lines, all starting from a point
    ``seed`` (the pull request's number) moves.

    Args:
        ranges: The changed lines per file, as passed to Stryker.
        mutated: The mutant lines per file from that run's report.
        sources: Each changed file's text, by path; a file missing here is
            not widened.
        seed: Varies the sample between pull requests.
        budget: The most lines chosen in all.

    Returns:
        The chosen lines per widened file, sorted; empty when every file's
        changed lines gave a mutant.
    """
    candidates: dict[str, list[int]] = {}
    for path, runs in ranges.items():
        if path not in sources:
            continue
        changed = {line for start, end in runs for line in range(start, end + 1)}
        if mutated.get(path, set()) & changed:
            continue
        usable = [line for line in sample_lines(sources[path]) if line not in changed]
        if usable:
            candidates[path] = usable
    chosen: dict[str, set[int]] = {path: set() for path in candidates}
    files = sorted(candidates)
    left = max(budget, 0)
    for index in range(len(files)):
        if left == 0:
            break
        path = files[(seed + index) % len(files)]
        options = candidates[path]
        chosen[path].add(options[seed % len(options)])
        left -= 1
    pool = [(path, line) for path in files for line in candidates[path] if line not in chosen[path]]
    take = min(left, len(pool))
    start = seed % len(pool) if pool else 0
    # Evenly spaced and distinct, since ``take`` is at most the pool's size.
    for step in range(take):
        path, line = pool[(start + step * len(pool) // take) % len(pool)]
        chosen[path].add(line)
    return {path: sorted(lines) for path, lines in chosen.items() if lines}


def render(
    name: str,
    parsed: tuple[Results, list[tuple[str, int, str]]] | None,
    *,
    mutated: bool,
    stopped: bool = False,
    widened: dict[str, int] | None = None,
    unwidened: list[str] | None = None,
) -> str:
    """Write the Markdown summary of one Worker's run.

    Args:
        name: The Worker's name, for the heading.
        parsed: The parsed report, or ``None`` when Stryker left none.
        mutated: Whether the pull request changed any of the Worker's
            source lines, so Stryker ran.
        stopped: Whether the workflow stopped Stryker at its time limit, so
            the report holds only the mutants tested before then.
        widened: The files whose changed lines gave no mutant, with how many
            of their other lines were sampled and mutated too.
        unwidened: Files whose changed lines gave no mutant but whose sample
            was not tested: too little time was left, or the second pass
            failed.

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
        if widened:
            names = ", ".join(f"{_code(_clean(path))} ({count} lines)" for path, count in sorted(widened.items()))
            lines += [
                "No mutant came from some changed lines, as with a type, an import or a deletion, so a sample of the other"
                f" lines in those files was mutated too, chosen afresh for each pull request: {names}.",
                "",
            ]
        if unwidened:
            names = ", ".join(_code(_clean(path)) for path in sorted(unwidened))
            lines += [
                f"No mutant came from the changed lines of {names}, and the sample of their other lines was not tested:"
                " too little time was left, or the second pass failed (see the job log).",
                "",
            ]
        if stopped:
            lines += ["Stryker reached the time limit, so only the mutants tested before it are counted.", ""]
        score = results.score()
        if score is None and results.count(Outcome.UNVIABLE):
            lines.append("Stryker made mutants of the changed lines, but none could be tested (all unviable).")
        elif score is None:
            lines.append("Stryker found no mutant on the changed lines.")
        else:
            missed = results.count(Outcome.MISSED)
            scope = "mutants tested" if widened else "mutants of the changed lines"
            lines.append(f"The tests caught **{score:.0f}%** of the {results.tested} {scope} ({missed} missed).")
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


def widen_main(argv: list[str]) -> int:
    """Print the second pass's ``--mutate`` value, or nothing when none is needed.

    Args:
        argv: ``--report`` (the first pass's report), ``--mutate`` (its
            ``--mutate`` value), ``--root`` (the Worker's directory, which
            the paths are relative to), ``--seed`` and ``--list`` (where to
            write ``path<TAB>count`` per widened file, for the report).

    Returns:
        0. The value printed is the first pass's ranges plus the sampled
        lines, so incremental mode keeps the first pass's results.
    """
    parser = argparse.ArgumentParser(prog="stryker_report.py widen")
    parser.add_argument("--report", type=Path, required=True, help="the first pass's Stryker report")
    parser.add_argument("--mutate", required=True, help="the first pass's --mutate value")
    parser.add_argument("--root", type=Path, required=True, help="the Worker's directory")
    parser.add_argument("--seed", type=int, default=0, help="varies the sample; the pull request's number")
    parser.add_argument("--list", type=Path, help="file to write each widened file and its sampled line count to")
    args = parser.parse_args(argv)
    ranges = parse_mutate(args.mutate)
    report = args.report.read_text(encoding="utf-8", errors="replace") if args.report.is_file() and args.report.stat().st_size <= MAX_REPORT_BYTES else ""
    mutated = mutant_lines(report)
    if mutated is None:
        return 0
    sources = {}
    for path in ranges:
        file = args.root / path
        if file.is_file() and file.stat().st_size <= MAX_REPORT_BYTES:
            sources[path] = file.read_text(encoding="utf-8", errors="replace")
    chosen = widen(ranges, mutated, sources, args.seed)
    if not chosen:
        return 0
    extra = {path: [(line, line) for line in lines] for path, lines in chosen.items()}
    merged = {path: ranges.get(path, []) + extra.get(path, []) for path in {**ranges, **extra}}
    sys.stdout.write(mutate_patterns(dict(sorted(merged.items()))) + "\n")
    if args.list is not None:
        args.list.write_text("".join(f"{path}\t{len(lines)}\n" for path, lines in sorted(chosen.items())), encoding="utf-8")
    return 0


def parse_widened(text: str) -> dict[str, int]:
    """Read the list ``widen --list`` wrote.

    Args:
        text: ``path<TAB>count`` per line, at most 10000 lines read.

    Returns:
        The sampled line count per Worker-relative path; lines whose path
        fails ``SAFE_PATH`` or whose count is not a number below a million
        are left out.
    """
    out: dict[str, int] = {}
    for line in text.splitlines()[:10000]:
        path, _, count = line.partition("\t")
        if SAFE_PATH.match(path) and count.isdigit() and len(count) < 7:
            out[path] = int(count)
    return out


def report_main(argv: list[str]) -> int:
    """Write one Worker's summary and annotations."""
    parser = argparse.ArgumentParser(prog="stryker_report.py report")
    parser.add_argument("--report", type=Path, required=True, help="Stryker's JSON report")
    parser.add_argument("--root", required=True, help="the Worker's directory, relative to the repository root")
    parser.add_argument("--name", required=True, help="the Worker's name, for the heading and artifact")
    parser.add_argument("--not-mutated", action="store_true", help="the change touched no source line, so Stryker did not run")
    parser.add_argument("--stopped", action="store_true", help="the workflow stopped Stryker at its time limit")
    parser.add_argument("--widened", type=Path, help="the list `widen --list` wrote")
    parser.add_argument("--unwidened", type=Path, help="the same list, when its sample was not tested")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""), help="file to append the summary to (none when empty)")
    args = parser.parse_args(argv)
    parsed = None
    if not args.not_mutated and args.report.is_file() and args.report.stat().st_size <= MAX_REPORT_BYTES:
        parsed = parse_report(args.report.read_text(encoding="utf-8", errors="replace"), args.root)
    widened = parse_widened(args.widened.read_text(encoding="utf-8", errors="replace")) if args.widened is not None and args.widened.is_file() else {}
    prefix = args.root.rstrip("/") + "/"
    widened = {prefix + path: count for path, count in widened.items()}
    unwidened_text = args.unwidened.read_text(encoding="utf-8", errors="replace") if args.unwidened is not None and args.unwidened.is_file() else ""
    unwidened = [prefix + path for path in parse_widened(unwidened_text)]
    markdown = render(_clean(args.name), parsed, mutated=not args.not_mutated, stopped=args.stopped, widened=widened, unwidened=unwidened)
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
    commands = {"lines": lines_main, "widen": widen_main, "report": report_main}
    if not argv or argv[0] not in commands:
        sys.stderr.write("usage: stryker_report.py {lines,widen,report} ...\n")
        return 2
    return commands[argv[0]](argv[1:])


if __name__ == "__main__":
    sys.exit(main())
