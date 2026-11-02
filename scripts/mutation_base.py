#!/usr/bin/env python3
"""Finds the commit a daily mutation testing run starts from.

The mutation testing workflows (``docs/contributing/ci.md#mutation-testing``)
run once a day on the default branch and mutate the code merged since their
last successful run, so a day the schedule skipped or a run that failed is
covered by the next one. This prints that run's commit, or, when there is
none to use (the first run, history rewritten, a token without access), the
last commit on the branch's first-parent line from before ``--hours`` ago,
or the parent of ``HEAD``.

The previous run's commit comes from the GitHub API through ``gh``, so it is
validated before git sees it: a full lowercase SHA that names ``HEAD`` or
one of its ancestors. ``HEAD`` itself is the answer when nothing merged since
the last run: the range is empty and the run mutates nothing. ``choose`` is sans-IO so
``scripts/tests/test_mutation_base.py`` drives the same code.

Usage: ``mutation_base.py --workflow FILE [--base REV] [--hours N]`` from the
repository root of a full-history checkout, with ``GH_TOKEN`` and
``GITHUB_REPOSITORY`` set. ``--base`` (a manual run's input) wins when it
names an ancestor of ``HEAD``.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from collections.abc import Callable

#: A full commit SHA as the API returns it.
SHA = re.compile(r"^[0-9a-f]{40}$")

#: A revision a person may type for ``--base``: a SHA (short or full) or a
#: simple ref name, never an option.
REVISION = re.compile(r"^[0-9A-Za-z][0-9A-Za-z._/^~-]{0,99}$")

#: A workflow file name under ``.github/workflows``.
WORKFLOW = re.compile(r"^[A-Za-z0-9_-]{1,100}\.ya?ml$")


def choose(candidates: list[str], is_ancestor: Callable[[str], bool]) -> str | None:
    """Pick the first candidate that is a usable starting commit.

    Args:
        candidates: Possible starting commits, best first: a manual base, the
            last successful run's commit, the fallback.
        is_ancestor: Says whether a full SHA is ``HEAD`` or a commit ``HEAD``
            descends from.

    Returns:
        The first candidate that is a full SHA and an ancestor, or ``None``.
    """
    for candidate in candidates:
        text = candidate.strip()
        if SHA.match(text) and is_ancestor(text):
            return text
    return None


def _git(*args: str) -> str | None:
    """Run git and return its trimmed output, or ``None`` when it fails."""
    try:
        result = subprocess.run(["git", *args], capture_output=True, text=True, check=False, timeout=60)
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def _is_ancestor(sha: str) -> bool:
    """Say whether ``sha`` is ``HEAD`` or one of its ancestors."""
    try:
        result = subprocess.run(["git", "merge-base", "--is-ancestor", sha, "HEAD"], capture_output=True, check=False, timeout=60)
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


def _last_success(workflow: str) -> str | None:
    """Return the commit of the workflow's last successful run on this branch.

    Returns:
        The API's answer (validated later by ``choose``), ``""`` when the
        workflow has no successful run, or ``None`` when the lookup failed.
        A failed lookup must not fall back to a recent commit, or the
        changes between the last run and that commit would never be tested.
    """
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    branch = os.environ.get("GITHUB_REF_NAME", "main")
    if not re.match(r"^[A-Za-z0-9_.-]{1,100}/[A-Za-z0-9_.-]{1,100}$", repository) or not REVISION.match(branch):
        return None
    url = f"repos/{repository}/actions/workflows/{workflow}/runs?branch={branch}&status=success&per_page=1"
    try:
        result = subprocess.run(
            ["gh", "api", url, "--jq", ".workflow_runs[0].head_sha // empty"],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout.strip()[:100] if result.returncode == 0 else None


def main(argv: list[str] | None = None) -> int:
    """Print the starting commit.

    Args:
        argv: Command-line arguments; ``sys.argv`` when ``None``.

    Returns:
        0 with the commit printed, or 1 when no commit could be found or
        the last run could not be looked up (and no manual base applies).
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--workflow", required=True, help="this workflow's file name, such as mutation.yml")
    parser.add_argument("--base", default="", help="a manual run's starting revision")
    parser.add_argument("--hours", type=int, default=25, help="how far back the fallback reaches")
    args = parser.parse_args(argv)
    if not WORKFLOW.match(args.workflow):
        sys.stderr.write("mutation_base.py: not a workflow file name\n")
        return 1
    candidates = []
    if args.base and REVISION.match(args.base):
        candidates.append(_git("rev-parse", "--verify", "--end-of-options", f"{args.base}^{{commit}}") or "")
        manual = choose(candidates, _is_ancestor)
        if manual is not None:
            sys.stdout.write(manual + "\n")
            return 0
    last = _last_success(args.workflow)
    if last is None:
        sys.stderr.write("mutation_base.py: could not look up the last successful run\n")
        return 1
    candidates.append(last)
    hours = min(max(args.hours, 1), 24 * 30)
    candidates.append(_git("rev-list", "-1", "--first-parent", f"--before={hours} hours ago", "HEAD") or "")
    candidates.append(_git("rev-parse", "HEAD^1") or "")
    chosen = choose(candidates, _is_ancestor)
    if chosen is None:
        sys.stderr.write("mutation_base.py: no starting commit\n")
        return 1
    sys.stdout.write(chosen + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
