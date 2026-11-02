#!/usr/bin/env python3
"""Chooses the languages the CodeQL workflow analyzes on one run.

``.github/workflows/codeql.yml`` analyzes GitHub Actions workflows,
JavaScript/TypeScript, Python and Rust. Rust is the slow one (about 20
minutes on a hosted runner, and CodeQL has no incremental analysis for it
yet), so a pull request that changes no Rust input skips it: such a change
cannot add or fix a Rust alert. Pushes to ``main``, the weekly schedule and
manual runs analyze every language, so ``main`` always has a full analysis
to compare pull requests against. See
``docs/contributing/ci.md#codeql``.

The changed paths come from ``git diff -z --no-renames --name-only`` of the
pull request's merge commit against its base parent, run in the same job.
The file names are the pull request author's, so this is an untrusted-input
boundary (docs/contributing/fuzzing.md): the list is NUL-separated, so no
quoting can hide a ``.rs`` suffix, renames list both names, and a name that
is not UTF-8 counts as a Rust input. Parsing is sans-IO so
``scripts/tests/test_codeql_languages.py`` replays its seed corpus and fuzzes
the same code against an independent oracle.

Usage: ``codeql_languages.py --event NAME [--paths-file FILE]`` from the
repository root; writes ``matrix=<json>`` to ``$GITHUB_OUTPUT`` (or stdout
without it) and a one-line reason to ``$GITHUB_STEP_SUMMARY`` when set.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import sys
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path

# Languages analyzed on every run, with their CodeQL build mode. None of them
# is compiled, so `none` reads the sources directly.
ALWAYS: tuple[str, ...] = ("actions", "javascript-typescript", "python")

# The slow language and the inputs that change what its analysis sees: Rust
# sources, the manifests and lockfile that pick the crates and features, the
# toolchain pin, Cargo's configuration, and the Cap'n Proto schemas that
# generate Rust. This workflow and this script are inputs too, so a change to
# how Rust is analyzed is analyzed.
RUST = "rust"
RUST_INPUTS: tuple[str, ...] = (
    "*.rs",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/*",
    "*.capnp",
    ".github/workflows/codeql.yml",
    "scripts/codeql_languages.py",
)

# Events that always analyze every language.
FULL_EVENTS = frozenset({"push", "schedule", "workflow_dispatch"})


@dataclass(frozen=True)
class Choice:
    """The languages one run analyzes and why.

    Attributes:
        languages: CodeQL language names, in matrix order.
        reason: One sentence for the job summary.
    """

    languages: tuple[str, ...]
    reason: str


def is_rust_input(path: str) -> bool:
    """Whether a changed repository path can change the Rust analysis.

    Args:
        path: A repository-relative path with ``/`` separators, as
            ``git diff --name-only`` prints it.

    Returns:
        True when the path's file name or full path matches one of
        ``RUST_INPUTS``. A pattern without ``/`` matches the file name in any
        directory; a pattern with ``/`` matches the whole path.
    """
    name = path.rsplit("/", 1)[-1]
    for pattern in RUST_INPUTS:
        subject = path if "/" in pattern else name
        if fnmatch.fnmatchcase(subject, pattern):
            return True
    return False


def parse_paths(raw: bytes) -> list[str | None]:
    """Splits ``git diff -z --name-only`` output into paths.

    Args:
        raw: NUL-separated repository paths, as git wrote them.

    Returns:
        One entry per non-empty path: the path decoded as UTF-8, or None
        when it is not valid UTF-8 (the caller treats that as a Rust input,
        since it cannot tell).
    """
    out: list[str | None] = []
    for chunk in raw.split(b"\0"):
        if not chunk:
            continue
        try:
            out.append(chunk.decode("utf-8"))
        except UnicodeDecodeError:
            out.append(None)
    return out


def choose(event: str, paths: Iterable[str | None]) -> Choice:
    """Picks the languages for one CodeQL run.

    Args:
        event: The ``github.event_name`` that started the run.
        paths: The paths the pull request changes, None for a path that is
            not UTF-8; ignored for other events.

    Returns:
        Every language for pushes, schedules and manual runs, and for any
        event this script does not know (fail toward analyzing). A pull
        request gets Rust when at least one path is a Rust input or is not
        UTF-8.
    """
    if event != "pull_request":
        why = "full analysis" if event in FULL_EVENTS else f"unknown event {event!r}: full analysis"
        return Choice((*ALWAYS, RUST), why)
    listed = list(paths)
    if any(p is None for p in listed):
        return Choice((*ALWAYS, RUST), "Rust analyzed: the pull request changes a path that is not UTF-8")
    hits = sorted({p for p in listed if p and is_rust_input(p)})
    if hits:
        more = f" and {len(hits) - 1} more" if len(hits) > 1 else ""
        return Choice((*ALWAYS, RUST), f"Rust analyzed: the pull request changes {hits[0]}{more}")
    return Choice(ALWAYS, "Rust skipped: the pull request changes no Rust input")


def matrix_json(choice: Choice) -> str:
    """Renders a choice as the job matrix the workflow reads.

    Args:
        choice: The languages to analyze.

    Returns:
        Compact JSON ``{"include": [{"language": ..., "build-mode": "none"}]}``.
    """
    include = [{"language": lang, "build-mode": "none"} for lang in choice.languages]
    return json.dumps({"include": include}, separators=(",", ":"))


def main(argv: Sequence[str] | None = None) -> int:
    """Command-line entry point.

    Args:
        argv: Arguments without the program name; ``sys.argv[1:]`` when None.

    Returns:
        0 on success, 2 when ``--paths-file`` cannot be read.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--event", required=True, help="github.event_name")
    parser.add_argument("--paths-file", type=Path, help="changed paths, NUL-separated (git diff -z)")
    args = parser.parse_args(argv)
    paths: list[str | None] = []
    if args.paths_file is not None:
        try:
            paths = parse_paths(args.paths_file.read_bytes())
        except OSError as err:
            print(f"codeql_languages: cannot read {args.paths_file}: {err}", file=sys.stderr)
            return 2
    choice = choose(args.event, paths)
    line = f"matrix={matrix_json(choice)}\n"
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write(line)
    else:
        sys.stdout.write(line)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(f"CodeQL languages: {', '.join(choice.languages)}. {choice.reason}.\n")
    print(choice.reason, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
