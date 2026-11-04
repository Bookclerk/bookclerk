#!/usr/bin/env python3
"""Summarizes a cargo-mutants run for the mutation testing workflow.

Mutation testing checks that tests would catch a bug, not only that they run
the code: cargo-mutants makes small changes to the code merged since the
last daily run (a flipped comparison, a function that returns a default) and runs
the package's tests against each one. A mutant the tests still pass on is
"missed": the changed code has no test that checks that behavior. See
``docs/contributing/ci.md#mutation-testing``.

``.github/workflows/mutation.yml`` runs cargo-mutants and then this script,
which reads the run's ``mutants.out`` directory and writes a Markdown summary
to ``$GITHUB_STEP_SUMMARY``, and with ``--issue`` the issue body
``.github/workflows/mutation-issue.yml`` files when a mutant was missed. It
never blocks a merge. It exits 1 only when the run did not test its range
(the baseline failed or cargo-mutants errored), so the workflow fails and
the next daily run, which starts from the last successful one, covers the
same changes.

The lists are cargo-mutants' own ``caught.txt``, ``missed.txt``,
``timeout.txt`` and ``unviable.txt``, one mutant per line, written in the
same job. Parsing is sans-IO so ``scripts/tests/test_mutation_report.py``
drives the same code.

Mutation testing only changes code inside function bodies, so a change to
data a function reads (an attribute, a constant, a struct definition) yields
no mutants from the diff. ``mutation_report.py widen`` finds the changed Rust
files the diff gave no mutant, and the workflow then mutates the functions
in them, so the tests of the code beside the data are still checked.
``mutation_report.py sample`` rewrites the diff to mark the lines of those
files' mutants, all of them within a budget and an even sample past it.

Usage: ``mutation_report.py --out DIR --exit-code N [--stopped [REASON]] [--widened
FILE] [--sample FILE]`` from the repository root (``--summary FILE``
defaults to ``$GITHUB_STEP_SUMMARY``); ``mutation_report.py widen --diff
FILE --listed FILE`` prints the files to widen, one per line;
``mutation_report.py sample --diff FILE --listed FILE --budget N --seed N
--record FILE`` rewrites the diff for them.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import asdict, dataclass, field
from enum import Enum
from pathlib import Path

#: Most missed mutants listed in the summary; the rest are counted.
MAX_LISTED = 50

#: Longest list line kept, in characters; longer ones are cut.
MAX_LINE = 300

#: Largest list file read, in bytes; the rest is ignored.
MAX_LIST_BYTES = 4 * 1024 * 1024


class Outcome(Enum):
    """What happened to one mutant, named after cargo-mutants' list file."""

    #: A test failed: the tests catch this change.
    CAUGHT = "caught"
    #: Every test passed: no test checks this behavior.
    MISSED = "missed"
    #: The tests ran past their time limit; counted as caught.
    TIMEOUT = "timeout"
    #: The mutant did not build, so it says nothing about the tests.
    UNVIABLE = "unviable"


class Run(Enum):
    """How the cargo-mutants process ended, from its exit code."""

    #: Every mutant was caught (or there were none).
    CLEAN = "clean"
    #: Some mutants were missed (exit 2).
    MISSED = "missed"
    #: Some mutants timed out (exit 3).
    TIMEOUTS = "timeouts"
    #: The unmutated tree failed to build or its tests failed (exit 4).
    BASELINE_FAILED = "baseline failed"
    #: Any other exit: bad arguments, an unreadable diff or a crash.
    ERROR = "error"


class Stop(Enum):
    """Why the workflow stopped cargo-mutants before it tested every mutant."""

    #: The run reached the workflow's time limit.
    TIME_LIMIT = "time-limit"
    #: The runner's disk was nearly full; a full disk kills the runner and
    #: every result with it.
    LOW_DISK = "low-disk"
    #: The runner was nearly out of memory, which also kills the runner.
    LOW_MEMORY = "low-memory"


#: The note a stopped run's summary ends with, by why it stopped.
STOP_NOTES = {
    Stop.TIME_LIMIT: "The run reached its time limit, so some mutants were not tested.",
    Stop.LOW_DISK: "The runner ran low on disk space, so the run stopped early and some mutants were not tested.",
    Stop.LOW_MEMORY: "The runner ran low on memory, so the run stopped early and some mutants were not tested.",
}


#: cargo-mutants' documented exit codes.
EXIT_CODES = {0: Run.CLEAN, 2: Run.MISSED, 3: Run.TIMEOUTS, 4: Run.BASELINE_FAILED}


@dataclass
class Results:
    """The mutants of one run, by outcome, in the order cargo-mutants listed them."""

    #: Mutant descriptions (``path:line:col: replace … with …``) per outcome.
    mutants: dict[Outcome, list[str]] = field(default_factory=lambda: {o: [] for o in Outcome})

    def count(self, outcome: Outcome) -> int:
        """Return how many mutants had ``outcome``."""
        return len(self.mutants[outcome])

    @property
    def tested(self) -> int:
        """Mutants that built and ran the tests: caught, missed or timed out."""
        return self.count(Outcome.CAUGHT) + self.count(Outcome.MISSED) + self.count(Outcome.TIMEOUT)

    def score(self) -> float | None:
        """Return the share of tested mutants the tests caught, in percent.

        Returns:
            Caught and timed-out mutants over all tested ones, or ``None``
            when no mutant was tested.
        """
        if not self.tested:
            return None
        return 100.0 * (self.count(Outcome.CAUGHT) + self.count(Outcome.TIMEOUT)) / self.tested


def parse_list(text: str) -> list[str]:
    """Parse one cargo-mutants list file into mutant descriptions.

    Args:
        text: The file's contents: one mutant per line.

    Returns:
        The non-blank lines, stripped, with control characters replaced and
        each cut to ``MAX_LINE`` characters, so a line cannot break the
        Markdown summary or the log.
    """
    mutants = []
    for line in text.splitlines():
        line = "".join(c if c.isprintable() else "?" for c in line.strip())
        if line:
            mutants.append(line[:MAX_LINE])
    return mutants


def changed_rust_files(diff: str) -> list[str]:
    """List the Rust files a unified diff leaves in the new tree.

    Args:
        diff: The diff, as ``git diff`` writes it.

    Returns:
        The ``+++ b/<path>`` paths that end in ``.rs``, in diff order and
        without repeats; a deleted file (``+++ /dev/null``) is left out.
    """
    files: list[str] = []
    for line in diff.splitlines():
        if line.startswith("+++ b/") and line.endswith(".rs"):
            path = line[len("+++ b/") :]
            if path not in files:
                files.append(path)
    return files


def mutant_file(mutant: str) -> str:
    """Return the file a mutant description names.

    Args:
        mutant: A list line, ``path:line:col: replace …``.

    Returns:
        The path before the first ``:``, or the whole line when it has none.
    """
    return mutant.split(":", 1)[0]


def files_to_widen(diff: str, listed: str) -> list[str]:
    """List the changed Rust files the diff gave no mutant.

    Args:
        diff: The diff of the code being mutated.
        listed: The output of ``cargo mutants --list --in-diff`` on that
            diff: one mutant per line.

    Returns:
        Each changed Rust file no listed mutant is in, in diff order. Its
        changes are all outside function bodies (or in tests), so the
        workflow mutates the functions in it instead.
    """
    covered = {mutant_file(m) for m in parse_list(listed)}
    return [path for path in changed_rust_files(diff) if path not in covered]


@dataclass
class Sample:
    """Which mutants of the widened files a run tests."""

    #: Mutants cargo-mutants lists in the widened files.
    total: int
    #: Mutants the marked lines admit: all of them within the budget.
    tested: int


@dataclass(frozen=True)
class Span:
    """The lines one listed mutant covers; marking any of them admits it."""

    #: The file, relative to the repository root.
    path: str
    #: First line, 1-based.
    start: int
    #: Last line, at least ``start``.
    end: int


def parse_spans(listed: str) -> list[Span]:
    """Parse ``cargo mutants --list --json`` into the mutants' line spans.

    Args:
        listed: The JSON listing, written in the same job.

    Returns:
        One span per well-formed entry, in listing order; malformed entries
        and a malformed listing yield nothing.
    """
    try:
        entries = json.loads(listed)
    except ValueError:
        return []
    spans = []
    for entry in entries if isinstance(entries, list) else []:
        try:
            path, start, end = entry["file"], entry["span"]["start"]["line"], entry["span"]["end"]["line"]
        except (TypeError, KeyError):
            continue
        if isinstance(path, str) and path and type(start) is int and type(end) is int and 0 < start <= end < 10**7:
            spans.append(Span(path, start, end))
    return spans


def sample_lines(listed: str, budget: int, seed: int) -> tuple[dict[str, list[int]], Sample]:
    """Pick the lines of the widened files whose mutants a run tests.

    cargo-mutants tests every mutant whose span covers a marked line, so a
    line inside a function also admits its whole-body replacements. Within
    ``budget`` every mutant's first line is marked. Past it, each file first
    gets the line admitting the fewest mutants, while they fit; then
    candidate lines (each mutant's first line) are tried in an evenly spread
    order starting at ``seed``, and a line is kept only while the mutants
    admitted stay
    within ``budget``, so successive runs test different mutants of
    the same file and no run tests more than ``budget`` (unless every line
    alone admits more, when the line admitting the fewest is marked).

    Args:
        listed: ``cargo mutants --list --json`` over the widened files.
        budget: The most mutants to admit (at least 1).
        seed: Picks where the order starts; the workflow passes the pull
            request number.

    Returns:
        The marked lines per file, sorted, and what they admit.
    """
    spans = parse_spans(listed)
    budget = max(budget, 1)
    candidates = list(dict.fromkeys((span.path, span.start) for span in spans))
    if len(spans) <= budget:
        order = candidates
    else:
        count = len(candidates)
        stride = max(1, -(-count // budget))
        order = list(
            dict.fromkeys(candidates[(seed + j * stride + r) % count] for r in range(stride) for j in range(-(-count // stride)))
        )
    def covering(path: str, line: int) -> set[int]:
        return {i for i, span in enumerate(spans) if span.path == path and span.start <= line <= span.end}

    admitted: set[int] = set()
    marked: dict[str, set[int]] = {}

    def admit(path: str, line: int) -> None:
        new = covering(path, line) - admitted
        if len(admitted) + len(new) <= budget:
            admitted.update(new)
            marked.setdefault(path, set()).add(line)

    # Each file first gets the line that admits the fewest, while they fit,
    # so a file with few mutants is not crowded out by one with many.
    files = list(dict.fromkeys(path for path, _ in order))
    for index in range(len(files)):
        path = files[(seed + index) % len(files)]
        admit(*min((c for c in order if c[0] == path), key=lambda candidate: len(covering(*candidate))))
    for path, line in order:
        admit(path, line)
    if not admitted and order:
        # Every line alone admits more than the budget: test the smallest.
        path, line = min(order, key=lambda candidate: len(covering(*candidate)))
        admitted = covering(path, line)
        marked = {path: {line}}
    return {path: sorted(lines) for path, lines in marked.items()}, Sample(len(spans), len(admitted))


def drop_files(diff: str, paths: set[str]) -> str:
    """Remove the sections of a unified diff that change ``paths``.

    Args:
        diff: The diff, as ``git diff`` writes it.
        paths: New-side paths whose ``diff --git`` sections are dropped.

    Returns:
        The diff without those sections.
    """
    kept: list[str] = []
    skipping = False
    for line in diff.splitlines(keepends=True):
        if line.startswith("diff --git "):
            skipping = any(line.rstrip("\r\n").endswith(f" b/{path}") for path in paths)
        if not skipping:
            kept.append(line)
    return "".join(kept)


def line_hunks(path: str, source: str, lines: list[int]) -> str:
    """Write a diff section that marks ``lines`` of ``path`` as added.

    cargo-mutants tests the mutants on a diff's added lines and checks that
    each added line matches the file, so the section copies each selected
    line verbatim. Runs of consecutive lines share a hunk.

    Args:
        path: The file, relative to the repository root.
        source: Its contents.
        lines: The 1-based lines to mark, sorted; lines past the end are
            skipped.

    Returns:
        The section, or an empty string when no line is in the file.
    """
    # The same split as Rust's `str::lines`, which cargo-mutants compares with.
    text = source.split("\n")
    if text and text[-1] == "":
        text.pop()
    text = [t.removesuffix("\r") for t in text]
    runs: list[list[int]] = []
    for line in lines:
        if not 0 < line <= len(text):
            continue
        if runs and runs[-1][-1] + 1 == line:
            runs[-1].append(line)
        else:
            runs.append([line])
    if not runs:
        return ""
    out = [f"diff --git a/{path} b/{path}\n", f"--- a/{path}\n", f"+++ b/{path}\n"]
    for run in runs:
        out.append(f"@@ -{run[0] - 1},0 +{run[0]},{len(run)} @@\n")
        out += [f"+{text[line - 1]}\n" for line in run]
    return "".join(out)


def run_state(exit_code: int) -> Run:
    """Map cargo-mutants' exit code to how the run ended."""
    return EXIT_CODES.get(exit_code, Run.ERROR)


def usable(run: Run, stopped: Stop | None) -> bool:
    """Say whether a run tested the range it was given.

    The next daily run starts from the commit of the last successful one,
    so a run that failed before testing its mutants must fail the workflow,
    or the changes it should have tested would never be tested. A run the
    time limit stopped counts: what it tested stands, and retrying the same
    range would stop at the same place. So does a run stopped because the
    runner ran low on disk or memory: the same range would run out again.

    Args:
        run: How cargo-mutants ended.
        stopped: Why the workflow stopped it, or ``None`` when it ran to
            the end.

    Returns:
        ``False`` when the baseline failed or cargo-mutants exited with an
        error on its own.
    """
    return stopped is not None or run not in (Run.BASELINE_FAILED, Run.ERROR)


def read_results(out: Path) -> Results:
    """Read the list files of a ``mutants.out`` directory.

    Args:
        out: The directory; a missing list file counts as empty.

    Returns:
        The mutants by outcome.
    """
    results = Results()
    for outcome in Outcome:
        path = out / f"{outcome.value}.txt"
        if path.is_file():
            with path.open("rb") as handle:
                data = handle.read(MAX_LIST_BYTES)
            results.mutants[outcome] = parse_list(data.decode("utf-8", errors="replace"))
    return results


def _code(text: str) -> str:
    """Quote ``text`` as Markdown inline code, whatever backticks it holds.

    The fence is one backtick longer than the longest run of backticks in
    ``text``, padded with spaces when ``text`` holds any.
    """
    longest, run = 0, 0
    for char in text:
        run = run + 1 if char == "`" else 0
        longest = max(longest, run)
    fence = "`" * (longest + 1)
    return f"{fence} {text} {fence}" if longest else f"`{text}`"


def render(
    results: Results, run: Run, *, stopped: Stop | None, widened: list[str] | None = None, sample: Sample | None = None
) -> str:
    """Write the Markdown summary of a run.

    Args:
        results: The mutants by outcome.
        run: How the cargo-mutants process ended.
        stopped: Why the workflow stopped cargo-mutants before it tested
            every mutant, or ``None`` when it ran to the end.
        widened: The files whose functions were mutated because the
            diff changed them outside any function; only those that yielded
            a mutant are named.
        sample: Which of the widened files' mutants were selected, when
            they were sampled.

    Returns:
        The summary, ending with a newline.
    """
    lines = ["## Mutation testing (Rust)", ""]
    mutated = {mutant_file(m) for outcome in Outcome for m in results.mutants[outcome]}
    named = [path for path in widened or [] if path in mutated]
    if named:
        intro = (
            "These files changed only outside function bodies (attributes, types, constants or tests), "
            "so the functions in them were mutated instead:"
        )
        lines += [intro, ""]
        lines += [f"- {_code(path)}" for path in named[:MAX_LISTED]]
        lines.append("")
        if sample is not None and sample.tested < sample.total:
            ran = sum(
                mutant_file(m) in named for outcome in (Outcome.CAUGHT, Outcome.MISSED, Outcome.TIMEOUT) for m in results.mutants[outcome]
            )
            note = (
                f"The widened files, including any not listed, hold {sample.total} mutants, more than one run tests, "
                f"so {ran} spread across them were tested; the choice changes with each run."
            )
            lines += [note, ""]
    if run is Run.BASELINE_FAILED:
        lines.append("The unmutated code failed to build or its tests failed, so no mutant was tested. Check the job log.")
    elif run is Run.ERROR and not results.tested:
        lines.append("cargo-mutants did not finish; check the job log.")
    elif not results.tested and not results.count(Outcome.UNVIABLE):
        lines.append(
            "No mutants to test: the change holds no Rust function body cargo-mutants mutates. "
            "Attributes, type definitions, comments, tests and excluded files are not mutated."
        )
    else:
        score = results.score()
        if score is not None:
            lines.append(
                f"The tests caught **{score:.0f}%** of the {results.tested} mutants of the changed code "
                f"({results.count(Outcome.MISSED)} missed)."
            )
        lines += [
            "",
            "| Caught | Missed | Timed out | Unviable |",
            "| ---: | ---: | ---: | ---: |",
            "| " + " | ".join(str(results.count(o)) for o in (Outcome.CAUGHT, Outcome.MISSED, Outcome.TIMEOUT, Outcome.UNVIABLE)) + " |",
        ]
        missed = results.mutants[Outcome.MISSED]
        if missed:
            lines += ["", "Missed mutants: the tests still pass with each of these changes.", ""]
            lines += [f"- {_code(m)}" for m in missed[:MAX_LISTED]]
            if len(missed) > MAX_LISTED:
                lines.append(f"- … and {len(missed) - MAX_LISTED} more (see the `mutants-rust` artifact).")
    if stopped is not None:
        lines += ["", "> [!NOTE]", f"> {STOP_NOTES[stopped]}"]
    elif run is Run.ERROR and (results.tested or results.count(Outcome.UNVIABLE)):
        lines += [
            "",
            "> [!WARNING]",
            "> cargo-mutants exited with an error before it finished, so these results are incomplete. Check the job log.",
        ]
    lines += ["", "Daily run: missed mutants are filed as an issue and never block a merge."]
    return "\n".join(lines) + "\n"


def widen_main(argv: list[str]) -> int:
    """Print the changed Rust files to mutate whole, one per line.

    Args:
        argv: The arguments after ``widen``.

    Returns:
        0.
    """
    parser = argparse.ArgumentParser(prog="mutation_report.py widen", description=files_to_widen.__doc__.split("\n", 1)[0])
    parser.add_argument("--diff", type=Path, required=True, help="the diff of the code merged since the last run")
    parser.add_argument("--listed", type=Path, required=True, help="output of cargo mutants --list --in-diff")
    args = parser.parse_args(argv)
    diff = args.diff.read_text(encoding="utf-8", errors="replace")
    listed = args.listed.read_text(encoding="utf-8", errors="replace")
    for path in files_to_widen(diff, listed):
        sys.stdout.write(path + "\n")
    return 0


def sample_main(argv: list[str]) -> int:
    """Rewrite the diff so it selects a sample of the widened files' mutants.

    Args:
        argv: The arguments after ``sample``.

    Returns:
        0.
    """
    parser = argparse.ArgumentParser(prog="mutation_report.py sample", description=sample_lines.__doc__.split("\n", 1)[0])
    parser.add_argument("--diff", type=Path, required=True, help="the run's diff, rewritten in place")
    parser.add_argument("--listed", type=Path, required=True, help="cargo mutants --list --json over the widened files")
    parser.add_argument("--budget", type=int, required=True, help="the most widened mutants to select")
    parser.add_argument("--seed", type=int, required=True, help="picks the sample (the run number)")
    parser.add_argument("--record", type=Path, required=True, help="where to write the sample for the report (JSON)")
    args = parser.parse_args(argv)
    lines, sample = sample_lines(args.listed.read_text(encoding="utf-8", errors="replace"), args.budget, args.seed)
    diff = drop_files(args.diff.read_text(encoding="utf-8", errors="replace"), set(lines))
    for path, selected in lines.items():
        diff += line_hunks(path, Path(path).read_text(encoding="utf-8", errors="replace"), selected)
    args.diff.write_text(diff, encoding="utf-8")
    args.record.write_text(json.dumps(asdict(sample)), encoding="utf-8")
    return 0


def read_sample(text: str) -> Sample | None:
    """Parse the sample record ``sample`` wrote.

    Args:
        text: The JSON record.

    Returns:
        The sample, or ``None`` when the record is malformed.
    """
    try:
        data = json.loads(text)
        sample = Sample(int(data["total"]), int(data["tested"]))
    except (ValueError, KeyError, TypeError):
        return None
    return sample if 0 <= sample.tested <= sample.total else None


def write_issue(path: Path | None, markdown: str, results: Results | None) -> None:
    """Write the summary as an issue body when the run missed a mutant.

    The daily workflows file the body as an issue (one per area, updated by
    later runs), so a run with no missed mutant writes nothing.

    Args:
        path: Where to write the body; nothing is written when ``None``.
        markdown: The run's summary.
        results: The run's mutants, or ``None`` when it produced none.
    """
    if path is not None and results is not None and results.count(Outcome.MISSED):
        path.write_text(markdown, encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    """Write the summary of a cargo-mutants run, or run ``widen`` or ``sample``.

    Args:
        argv: Command-line arguments; ``sys.argv`` when ``None``.

    Returns:
        0, including when mutants were missed (they become an issue); 1 when
        the run did not test its range (``usable``), so the workflow fails
        and the next run covers the same changes.
    """
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["widen"]:
        return widen_main(argv[1:])
    if argv[:1] == ["sample"]:
        return sample_main(argv[1:])
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--out", type=Path, required=True, help="the run's mutants.out directory")
    parser.add_argument("--exit-code", type=int, required=True, help="cargo-mutants' exit code")
    parser.add_argument(
        "--stopped",
        nargs="?",
        const=Stop.TIME_LIMIT.value,
        choices=[stop.value for stop in Stop],
        help="the workflow stopped the run: at its time limit (the default) or because the runner ran low on disk or memory",
    )
    parser.add_argument("--widened", type=Path, help="files mutated whole, one per line (from widen)")
    parser.add_argument("--sample", type=Path, help="the sample record (from sample)")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""), help="file to append the summary to (none when empty)")
    parser.add_argument("--issue", type=Path, help="file to write the summary to when a mutant was missed, for the issue the workflow files")
    args = parser.parse_args(argv)
    widened = parse_list(args.widened.read_text(encoding="utf-8", errors="replace")) if args.widened and args.widened.is_file() else []
    sample = read_sample(args.sample.read_text(encoding="utf-8")) if args.sample and args.sample.is_file() else None
    results = read_results(args.out)
    run = run_state(args.exit_code)
    stopped = Stop(args.stopped) if args.stopped else None
    markdown = render(results, run, stopped=stopped, widened=widened, sample=sample)
    sys.stdout.write(markdown)
    write_issue(args.issue, markdown, results)
    if args.summary:
        with Path(args.summary).open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    return 0 if usable(run, stopped) else 1


if __name__ == "__main__":
    sys.exit(main())
