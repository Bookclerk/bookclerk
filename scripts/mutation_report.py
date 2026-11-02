#!/usr/bin/env python3
"""Summarizes a cargo-mutants run for the mutation testing workflow.

Mutation testing checks that tests would catch a bug, not only that they run
the code: cargo-mutants makes small changes to the code a pull request
touches (a flipped comparison, a function that returns a default) and runs
the package's tests against each one. A mutant the tests still pass on is
"missed": the changed code has no test that checks that behavior. See
``docs/contributing/ci.md#mutation-testing``.

``.github/workflows/mutation.yml`` runs cargo-mutants and then this script,
which reads the run's ``mutants.out`` directory and writes a Markdown summary
to ``$GITHUB_STEP_SUMMARY``. The trial is report-only: the script always
exits 0.

The lists are cargo-mutants' own ``caught.txt``, ``missed.txt``,
``timeout.txt`` and ``unviable.txt``, one mutant per line, written in the
same job. Parsing is sans-IO so ``scripts/tests/test_mutation_report.py``
drives the same code.

Mutation testing only changes code inside function bodies, so a change to
data a function reads (an attribute, a constant, a struct definition) yields
no mutants from the diff. ``mutation_report.py widen`` finds the changed Rust
files the diff gave no mutant, and the workflow then mutates every function
in them, so the tests of the code beside the data are still checked.

Usage: ``mutation_report.py --out DIR --exit-code N [--stopped] [--widened
FILE]`` from the repository root (``--summary FILE`` defaults to
``$GITHUB_STEP_SUMMARY``); ``mutation_report.py widen --diff FILE --listed
FILE`` prints the files to widen, one per line.
"""

from __future__ import annotations

import argparse
import os
import sys
from dataclasses import dataclass, field
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
        diff: The pull request's diff.
        listed: The output of ``cargo mutants --list --in-diff`` on that
            diff: one mutant per line.

    Returns:
        Each changed Rust file no listed mutant is in, in diff order. Its
        changes are all outside function bodies (or in tests), so the
        workflow mutates every function in it instead.
    """
    covered = {mutant_file(m) for m in parse_list(listed)}
    return [path for path in changed_rust_files(diff) if path not in covered]


def run_state(exit_code: int) -> Run:
    """Map cargo-mutants' exit code to how the run ended."""
    return EXIT_CODES.get(exit_code, Run.ERROR)


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


def render(results: Results, run: Run, *, stopped: bool, widened: list[str] | None = None) -> str:
    """Write the Markdown summary of a run.

    Args:
        results: The mutants by outcome.
        run: How the cargo-mutants process ended.
        stopped: Whether the workflow stopped cargo-mutants at its time
            limit, so later mutants were not tested.
        widened: The files whose every function was mutated because the
            diff changed them outside any function; only those that yielded
            a mutant are named.

    Returns:
        The summary, ending with a newline.
    """
    lines = ["## Mutation testing (Rust)", ""]
    mutated = {mutant_file(m) for outcome in Outcome for m in results.mutants[outcome]}
    named = [path for path in widened or [] if path in mutated]
    if named:
        intro = (
            "These files changed only outside function bodies (attributes, types, constants or tests), "
            "so every function in them was mutated:"
        )
        lines += [intro, ""]
        lines += [f"- {_code(path)}" for path in named[:MAX_LISTED]]
        lines.append("")
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
    if stopped:
        lines += ["", "> [!NOTE]", "> The run reached its time limit, so some mutants were not tested."]
    elif run is Run.ERROR and (results.tested or results.count(Outcome.UNVIABLE)):
        lines += [
            "",
            "> [!WARNING]",
            "> cargo-mutants exited with an error before it finished, so these results are incomplete. Check the job log.",
        ]
    lines += ["", "Report only: missed mutants do not fail the pull request yet."]
    return "\n".join(lines) + "\n"


def widen_main(argv: list[str]) -> int:
    """Print the changed Rust files to mutate whole, one per line.

    Args:
        argv: The arguments after ``widen``.

    Returns:
        0.
    """
    parser = argparse.ArgumentParser(prog="mutation_report.py widen", description=files_to_widen.__doc__.split("\n", 1)[0])
    parser.add_argument("--diff", type=Path, required=True, help="the pull request's diff")
    parser.add_argument("--listed", type=Path, required=True, help="output of cargo mutants --list --in-diff")
    args = parser.parse_args(argv)
    diff = args.diff.read_text(encoding="utf-8", errors="replace")
    listed = args.listed.read_text(encoding="utf-8", errors="replace")
    for path in files_to_widen(diff, listed):
        sys.stdout.write(path + "\n")
    return 0


def main(argv: list[str] | None = None) -> int:
    """Write the summary of a cargo-mutants run, or run ``widen``.

    Args:
        argv: Command-line arguments; ``sys.argv`` when ``None``.

    Returns:
        0: the trial never fails the workflow.
    """
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["widen"]:
        return widen_main(argv[1:])
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--out", type=Path, required=True, help="the run's mutants.out directory")
    parser.add_argument("--exit-code", type=int, required=True, help="cargo-mutants' exit code")
    parser.add_argument("--stopped", action="store_true", help="the run was stopped at its time limit")
    parser.add_argument("--widened", type=Path, help="files mutated whole, one per line (from widen)")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""), help="file to append the summary to (none when empty)")
    args = parser.parse_args(argv)
    widened = parse_list(args.widened.read_text(encoding="utf-8", errors="replace")) if args.widened and args.widened.is_file() else []
    markdown = render(read_results(args.out), run_state(args.exit_code), stopped=args.stopped, widened=widened)
    sys.stdout.write(markdown)
    if args.summary:
        with Path(args.summary).open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    return 0


if __name__ == "__main__":
    sys.exit(main())
