#!/usr/bin/env python3
"""Finds main's Rust CodeQL results for a pull request's base commit.

A pull request that changes no Rust input (``scripts/codeql_languages.py``
decides) would get exactly the Rust results ``main`` got for the commit the
pull request merges into. Instead of a 20-minute re-analysis, the
``Analyze (rust)`` job of ``.github/workflows/codeql.yml`` uploads that
analysis's SARIF again for the pull request. Code scanning then sees the
same Rust alerts on both sides: nothing new and nothing fixed, which is
what a re-analysis would report. See ``docs/contributing/ci.md#codeql``.

Only an exact match is reused. The run must be a ``codeql.yml`` run on
``main`` started by a push, the schedule or a manual dispatch (never a pull
request), for exactly the base commit, and its ``Analyze (rust)`` job must
have succeeded. The SARIF it kept as the ``codeql-rust-sarif`` artifact
must look like a CodeQL Rust analysis. Anything else prints why and writes
no output, and the job falls back to analyzing Rust.

The selection and the SARIF check are sans-IO functions over the bytes the
GitHub API returns, so ``scripts/tests/test_codeql_reuse.py`` drives the
same code the workflow does.

Usage: ``codeql_reuse.py --repo OWNER/NAME --base SHA --dest DIR`` with
``GH_TOKEN`` set; writes ``sarif=<path>`` to ``$GITHUB_OUTPUT`` (or stdout
without it) when the results can be reused, and one line to
``$GITHUB_STEP_SUMMARY`` either way.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path

# The workflow file whose runs are searched, the job whose result is reused,
# and the artifact that job keeps on main.
WORKFLOW_FILE = "codeql.yml"
RUST_JOB = "Analyze (rust)"
ARTIFACT = "codeql-rust-sarif"

# Events whose runs analyze every language on main's own code. Pull request
# runs are never trusted as a source: their workflow is the author's.
SOURCE_EVENTS = frozenset({"push", "schedule", "workflow_dispatch"})

# A full commit id, as `git rev-parse` prints it.
SHA_RE = re.compile(r"[0-9a-f]{40}")

# The `automationDetails.id` a Rust analysis carries: the category, which
# CodeQL writes with a trailing slash. `upload-sarif` keeps an id that is
# already there, so any other id would upload a different configuration.
RUST_CATEGORY = "/language:rust"
RUST_AUTOMATION_IDS = frozenset({RUST_CATEGORY, RUST_CATEGORY + "/"})

# Runs `gh` with the given arguments and returns its stdout; raises
# `subprocess.CalledProcessError` when it fails.
Gh = Callable[[Sequence[str]], bytes]


def candidate_runs(raw: bytes, base: str) -> list[int]:
    """Picks the main runs that analyzed exactly the base commit.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/workflows/{file}/runs``.
        base: The full commit id the pull request merges into.

    Returns:
        Ids of completed runs on ``main`` for ``base`` started by one of
        ``SOURCE_EVENTS``, newest first; empty when the body is not the
        expected shape.
    """
    try:
        doc = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return []
    runs = doc.get("workflow_runs") if isinstance(doc, dict) else None
    if not isinstance(runs, list):
        return []
    picked: list[tuple[str, int]] = []
    for run in runs:
        if not isinstance(run, dict):
            continue
        run_id = run.get("id")
        if (
            isinstance(run_id, int)
            and not isinstance(run_id, bool)
            and run.get("head_sha") == base
            and run.get("head_branch") == "main"
            and run.get("event") in SOURCE_EVENTS
            and run.get("status") == "completed"
        ):
            picked.append((str(run.get("created_at") or ""), run_id))
    picked.sort(reverse=True)
    return [run_id for _, run_id in picked]


def rust_job_succeeded(raw: bytes) -> bool:
    """Whether a run's Rust analysis job finished successfully.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}/jobs``.

    Returns:
        True when exactly one job is named ``RUST_JOB`` and it concluded
        ``success``.
    """
    try:
        doc = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return False
    jobs = doc.get("jobs") if isinstance(doc, dict) else None
    if not isinstance(jobs, list):
        return False
    rust = [j for j in jobs if isinstance(j, dict) and j.get("name") == RUST_JOB]
    return len(rust) == 1 and rust[0].get("conclusion") == "success"


def sarif_problem(raw: bytes) -> str | None:
    """Checks that a file is the SARIF of a CodeQL Rust analysis.

    Args:
        raw: The file's bytes.

    Returns:
        None when it is SARIF 2.1.0 with at least one run, every run's tool
        is CodeQL and every run's ``automationDetails.id`` is exactly the
        Rust category; otherwise a clause saying what is wrong.
    """
    try:
        doc = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return "the SARIF is not JSON"
    if not isinstance(doc, dict) or doc.get("version") != "2.1.0":
        return "the SARIF is not version 2.1.0"
    runs = doc.get("runs")
    if not isinstance(runs, list) or not runs:
        return "the SARIF has no runs"
    for run in runs:
        if not isinstance(run, dict):
            return "a SARIF run is not an object"
        tool = run.get("tool")
        driver = tool.get("driver") if isinstance(tool, dict) else None
        if not isinstance(driver, dict) or driver.get("name") != "CodeQL":
            return "a SARIF run is not from CodeQL"
        details = run.get("automationDetails")
        ident = details.get("id") if isinstance(details, dict) else None
        if ident not in RUST_AUTOMATION_IDS:
            return "a SARIF run does not carry the Rust category"
    return None


def find(repo: str, base: str, dest: Path, gh: Gh) -> tuple[Path | None, str]:
    """Downloads main's Rust SARIF for the base commit when it can be reused.

    Args:
        repo: ``owner/name``.
        base: The full commit id the pull request merges into.
        dest: An empty directory to download into.
        gh: Runs the GitHub CLI.

    Returns:
        The SARIF file and a reason, or None and why nothing is reused.
    """
    if not SHA_RE.fullmatch(base):
        return None, f"the base commit {base!r} is not a full commit id"
    try:
        runs = gh(
            [
                "api",
                "-X",
                "GET",
                f"repos/{repo}/actions/workflows/{WORKFLOW_FILE}/runs",
                "-f",
                f"head_sha={base}",
                "-f",
                "branch=main",
                "-f",
                "per_page=50",
            ]
        )
    except subprocess.CalledProcessError:
        return None, "listing main's CodeQL runs failed"
    ids = candidate_runs(runs, base)
    if not ids:
        return None, f"main has no finished CodeQL run for {base[:12]}"
    for run_id in ids:
        try:
            jobs = gh(["api", "-X", "GET", f"repos/{repo}/actions/runs/{run_id}/jobs", "-f", "per_page=100"])
        except subprocess.CalledProcessError:
            continue
        if not rust_job_succeeded(jobs):
            continue
        target = dest / str(run_id)
        try:
            gh(["run", "download", str(run_id), "-R", repo, "-n", ARTIFACT, "-D", str(target)])
        except subprocess.CalledProcessError:
            continue
        files = sorted(target.glob("*.sarif"))
        if len(files) != 1:
            continue
        problem = sarif_problem(files[0].read_bytes())
        if problem is not None:
            return None, f"run {run_id}: {problem}"
        return files[0], f"reused the Rust results of main's run {run_id} for {base[:12]}"
    return None, f"no CodeQL run on main kept Rust results for {base[:12]}"


def run_gh(args: Sequence[str]) -> bytes:
    """Runs the GitHub CLI and returns its stdout.

    Args:
        args: Arguments after ``gh``.

    Returns:
        The command's standard output.

    Raises:
        subprocess.CalledProcessError: When ``gh`` exits non-zero.
    """
    return subprocess.run(["gh", *args], check=True, capture_output=True).stdout


def main(argv: Sequence[str] | None = None, gh: Gh = run_gh) -> int:
    """Command-line entry point.

    Args:
        argv: Arguments without the program name; ``sys.argv[1:]`` when None.
        gh: Runs the GitHub CLI (replaced in tests).

    Returns:
        0 whether or not results are reused: falling back to an analysis is
        not an error.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True, help="owner/name")
    parser.add_argument("--base", required=True, help="the merge commit's base parent")
    parser.add_argument("--dest", required=True, type=Path, help="directory to download into")
    args = parser.parse_args(argv)
    args.dest.mkdir(parents=True, exist_ok=True)
    sarif, reason = find(args.repo, args.base, args.dest, gh)
    if sarif is not None:
        line = f"sarif={sarif}\n"
        out = os.environ.get("GITHUB_OUTPUT")
        if out:
            with open(out, "a", encoding="utf-8") as fh:
                fh.write(line)
        else:
            sys.stdout.write(line)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    verdict = reason if sarif is not None else f"Rust analyzed: {reason}"
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(f"CodeQL Rust: {verdict}.\n")
    print(verdict, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
