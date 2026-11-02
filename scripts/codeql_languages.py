#!/usr/bin/env python3
"""Chooses the languages the CodeQL workflow analyzes on one run.

``.github/workflows/codeql.yml`` analyzes GitHub Actions workflows,
JavaScript/TypeScript, Python and Rust. Rust is the slow one (about 20
minutes on a hosted runner, and CodeQL has no incremental analysis for it
yet). A pull request that changes no Rust input would get exactly the Rust
results ``main`` got for its base commit, so this script marks Rust for
reuse there (``PULL_REQUESTS_REUSE_RUST``): ``scripts/codeql_reuse.py``
uploads that analysis again, and the job analyzes Rust only when it can't.
Rust is never left out, because code scanning's merge rule blocks a pull
request that lacks a configuration ``main`` has. Pushes to ``main``, the
weekly schedule and manual runs analyze every language. See
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
repository root; writes ``matrix=<json>`` and ``reuse-rust=true|false`` to
``$GITHUB_OUTPUT`` (or stdout without it) and a one-line reason to ``$GITHUB_STEP_SUMMARY`` when set.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import posixpath
import re
import subprocess
import sys
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path

# Languages analyzed on every run, with their CodeQL build mode. None of them
# is compiled, so `none` reads the sources directly.
ALWAYS: tuple[str, ...] = ("actions", "javascript-typescript", "python")

# The slow language and the inputs that change what its analysis sees: Rust
# sources, the manifests and lockfile that pick the crates and features, the
# toolchain pin, Cargo's configuration (a `.cargo/` directory at any depth,
# see `CARGO_DIR`), and the schemas build scripts compile into Rust (Cap'n
# Proto for the plugin ABI, protobuf for audible-rs); CodeQL's `none` mode
# still runs build scripts. The workflow and the scripts that choose and reuse
# are inputs too, so a change to how Rust is analyzed is analyzed.
RUST = "rust"
RUST_INPUTS: tuple[str, ...] = (
    "*.rs",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "*.capnp",
    "*.proto",
    ".github/workflows/codeql.yml",
    "scripts/codeql_languages.py",
    "scripts/codeql_reuse.py",
)

# A directory holding Cargo configuration; any file under one, at any depth,
# is a Rust input (`fuzz/.cargo/config.toml` sets cfgs for that crate).
CARGO_DIR = ".cargo"

# Events that always analyze every language.
FULL_EVENTS = frozenset({"push", "schedule", "workflow_dispatch"})

# Whether a pull request that changes no Rust input reuses main's Rust results
# for its base commit instead of analyzing Rust. Rust is never skipped: code
# scanning's merge rule ("CodeQL results") reports a pull request that lacks
# a configuration main has (`/language:rust`) as neutral, "1 configuration
# not found", and a neutral result blocks the merge (#583).
PULL_REQUESTS_REUSE_RUST = True


# A macro that embeds another file's contents into a Rust crate. rust-analyzer
# reads that file, so CodeQL sees its contents as a Rust literal or source.
INCLUDE_RE = re.compile(r"\binclude(?:_str|_bytes)?!\s*\(")

# A plain string literal with no escapes, the only path form resolved as is.
PLAIN_LITERAL_RE = re.compile(r'"([^"\\]*)"')

# `concat!(env!("<VAR>"), "<path>")`, the only computed path form resolved.
CONCAT_ENV_RE = re.compile(r'concat!\(\s*env!\(\s*"([A-Z_]+)"\s*\)\s*,\s*"([^"\\]*)"\s*,?\s*\)')


@dataclass(frozen=True)
class RepoInputs:
    """Files outside ``RUST_INPUTS`` that still change the Rust analysis.

    Found by scanning the merge commit's tree (``scan_repo``), so a pull
    request reuses main's Rust results only when none of its paths is one.

    Attributes:
        files: Repository paths a Rust source embeds with ``include_str!``,
            ``include_bytes!`` or ``include!``.
        dirs: Directories of crates with a build script. A build script can
            read any file in its crate, so every path under one counts.
        unresolved: ``<source>: <argument>`` for each include whose path
            this script can't work out; any one turns reuse off.
    """

    files: frozenset[str]
    dirs: tuple[str, ...]
    unresolved: tuple[str, ...]

    def reason(self, paths: Iterable[str]) -> str | None:
        """Says why these paths, or the tree itself, rule out reuse.

        Args:
            paths: Changed repository paths, all UTF-8.

        Returns:
            A clause naming an unresolved include, an embedded file or a
            path in a build-script crate, or None when none applies.
        """
        if self.unresolved:
            return f"an include path can't be resolved ({self.unresolved[0]})"
        for path in sorted(paths):
            if path in self.files:
                return f"the pull request changes {path}, which a Rust source embeds"
            for d in self.dirs:
                if d == "" or path.startswith(d + "/"):
                    return f"the pull request changes {path}, in a crate with a build script"
        return None


def _include_args(text: str) -> list[str]:
    """Returns the argument text of every include macro call in a source.

    Args:
        text: A Rust source file. Comments are scanned too, which can only
            add paths, never hide one.

    Returns:
        The text between each call's parentheses, or the rest of the file
        when the parentheses never balance.
    """
    out: list[str] = []
    for match in INCLUDE_RE.finditer(text):
        depth = 1
        i = match.end()
        start = i
        while i < len(text) and depth:
            if text[i] == "(":
                depth += 1
            elif text[i] == ")":
                depth -= 1
            i += 1
        out.append(text[start : i - 1] if depth == 0 else text[start:])
    return out


def scan_repo(sources: Iterable[tuple[str, str]], manifests: Iterable[str], build_scripts: Iterable[str]) -> RepoInputs:
    """Finds the files outside ``RUST_INPUTS`` that the Rust analysis reads.

    Args:
        sources: ``(path, text)`` for every tracked ``.rs`` file.
        manifests: Paths of every tracked ``Cargo.toml``.
        build_scripts: Paths of every tracked ``build.rs``.

    Returns:
        The embedded files, the build-script crate directories, and the
        includes whose path is neither a plain literal relative to the
        source, ``CARGO_MANIFEST_DIR`` plus a literal, nor ``OUT_DIR``
        plus a literal in a crate with a build script.
    """
    crates = sorted({posixpath.dirname(m) for m in manifests}, key=len, reverse=True)
    script_dirs = tuple(sorted({posixpath.dirname(b) for b in build_scripts}))

    def crate_of(path: str) -> str | None:
        for crate in crates:
            if crate == "" or path.startswith(crate + "/"):
                return crate
        return None

    files: set[str] = set()
    unresolved: list[str] = []
    for path, text in sources:
        for arg in _include_args(text):
            arg = arg.strip()
            plain = PLAIN_LITERAL_RE.fullmatch(arg)
            computed = CONCAT_ENV_RE.fullmatch(arg)
            target: str | None = None
            if plain:
                target = posixpath.join(posixpath.dirname(path), plain.group(1))
            elif computed and computed.group(1) == "CARGO_MANIFEST_DIR":
                crate = crate_of(path)
                if crate is not None:
                    target = posixpath.join(crate, computed.group(2).lstrip("/"))
            elif computed and computed.group(1) == "OUT_DIR":
                crate = crate_of(path)
                if crate is not None and crate in script_dirs:
                    continue
            if target is None:
                unresolved.append(f"{path}: {' '.join(arg.split())[:80]}")
                continue
            files.add(posixpath.normpath(target))
    return RepoInputs(frozenset(files), script_dirs, tuple(unresolved))


def read_repo(root: Path) -> RepoInputs:
    """Scans the tracked files of a checkout with ``scan_repo``.

    Args:
        root: The repository's top directory.

    Returns:
        What ``scan_repo`` finds in the files ``git ls-files`` lists.

    Raises:
        subprocess.CalledProcessError: When ``git ls-files`` fails.
        OSError: When a listed file can't be read.
    """

    def listed(pattern: str) -> list[str]:
        raw = subprocess.run(
            ["git", "ls-files", "-z", "--", pattern], cwd=root, capture_output=True, check=True
        ).stdout
        return [p for p in raw.decode("utf-8", "surrogateescape").split("\0") if p]

    sources = [(p, (root / p).read_text(encoding="utf-8", errors="replace")) for p in listed("*.rs")]
    return scan_repo(sources, listed("*Cargo.toml"), listed("*build.rs"))


@dataclass(frozen=True)
class Choice:
    """The languages one run analyzes and why.

    Attributes:
        languages: CodeQL language names, in matrix order.
        reason: One sentence for the job summary.
        reuse_rust: Whether the Rust job may upload main's results for the
            base commit instead of analyzing; it still analyzes when those
            results can't be found.
    """

    languages: tuple[str, ...]
    reason: str
    reuse_rust: bool = False


def is_rust_input(path: str) -> bool:
    """Whether a changed repository path can change the Rust analysis.

    Args:
        path: A repository-relative path with ``/`` separators, as
            ``git diff --name-only`` prints it.

    Returns:
        True when the path is under a ``.cargo`` directory at any depth, or
        its file name or full path matches one of ``RUST_INPUTS``. A pattern
        without ``/`` matches the file name in any directory; a pattern with
        ``/`` matches the whole path.
    """
    parts = path.split("/")
    if CARGO_DIR in parts[:-1]:
        return True
    name = parts[-1]
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


def choose(event: str, paths: Iterable[str | None], repo: RepoInputs | None = None) -> Choice:
    """Picks the languages for one CodeQL run.

    Args:
        event: The ``github.event_name`` that started the run.
        paths: The paths the pull request changes, None for a path that is
            not UTF-8; ignored for other events.
        repo: What the merge commit's tree adds to ``RUST_INPUTS``; None
            when it wasn't scanned, which rules reuse out.

    Returns:
        Every language, always. Pushes, schedules, manual runs and any
        event this script does not know (fail toward analyzing) analyze
        Rust. A pull request whose paths are all UTF-8, none a Rust input
        by name or by ``repo``, may reuse main's Rust results while
        `PULL_REQUESTS_REUSE_RUST` is on.
    """
    if event != "pull_request":
        why = "full analysis" if event in FULL_EVENTS else f"unknown event {event!r}: full analysis"
        return Choice((*ALWAYS, RUST), why)
    listed = list(paths)
    why = rust_reason(listed)
    if why is not None:
        return Choice((*ALWAYS, RUST), f"Rust analyzed: {why}")
    if not PULL_REQUESTS_REUSE_RUST:
        return Choice((*ALWAYS, RUST), "Rust analyzed: reusing main's results is turned off")
    if repo is None:
        return Choice((*ALWAYS, RUST), "Rust analyzed: the tree was not scanned for embedded files")
    why = repo.reason(p for p in listed if p is not None)
    if why is not None:
        return Choice((*ALWAYS, RUST), f"Rust analyzed: {why}")
    return Choice(
        (*ALWAYS, RUST),
        "Rust may reuse main's results for the base commit: the pull request changes no Rust input",
        reuse_rust=True,
    )


def rust_reason(paths: Iterable[str | None]) -> str | None:
    """Says why a pull request's changed paths need the Rust analysis.

    Args:
        paths: The paths the pull request changes, None for a path that is
            not UTF-8.

    Returns:
        A clause naming the first Rust input, or a path that is not UTF-8,
        or None when no path can change the Rust analysis.
    """
    listed = list(paths)
    if any(p is None for p in listed):
        return "the pull request changes a path that is not UTF-8"
    hits = sorted({p for p in listed if p and is_rust_input(p)})
    if not hits:
        return None
    more = f" and {len(hits) - 1} more" if len(hits) > 1 else ""
    return f"the pull request changes {hits[0]}{more}"


def matrix_json(choice: Choice) -> str:
    """Renders a choice as the job matrix the workflow reads.

    Args:
        choice: The languages to analyze.

    Returns:
        Compact JSON ``{"include": [{"language": ..., "build-mode": "none"}]}``.
        It carries nothing else: code scanning records the matrix values
        with each analysis, so a pull request whose values differ from
        main's would not match main's configuration. Reuse travels as the
        separate ``reuse-rust`` output.
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
    repo: RepoInputs | None = None
    if args.event == "pull_request":
        try:
            repo = read_repo(Path.cwd())
        except (OSError, subprocess.CalledProcessError) as err:
            print(f"codeql_languages: cannot scan the tree, analyzing Rust: {err}", file=sys.stderr)
    choice = choose(args.event, paths, repo)
    reuse = "true" if choice.reuse_rust else "false"
    line = f"matrix={matrix_json(choice)}\nreuse-rust={reuse}\n"
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
