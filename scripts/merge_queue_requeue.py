#!/usr/bin/env python3
"""Recovers CI runs that failed only because GitHub never gave a job a runner.

GitHub sometimes never hands a merge-group job a hosted runner (most often
macOS arm64). The job sits queued, GitHub cancels it with the annotation
"The job was not acquired by Runner of type hosted even after multiple
attempts", ``CI Gate`` fails, and the queue removes the pull request with
every test that did run green. A pull request's own CI run fails the same
way. ``.github/workflows/merge-queue-requeue.yml`` runs this script when a
CI run fails.

For a ``merge_group`` run it turns auto-merge back on only when that is the
whole story:

- the run is a ``merge_group`` run of this repository's CI workflow, on a
  ``gh-readonly-queue/main/pr-<n>-<sha>`` branch;
- every job that did not succeed or skip is ``CI Gate``,
  ``merge-queue fail-fast`` or a job that never started (cancelled, no
  runner, no steps) and carries GitHub's not-acquired annotation; at least
  one job is of the last kind. A job cancelled because someone removed the
  pull request from the queue has no such annotation, so it never counts;
- the pull request is open, not a draft, has no auto-merge set, and comes
  from this repository;
- its current head is the one that was queued (it entered the queue at
  least once since its head last changed, so a commit pushed after the
  failure is never enqueued on the failure's account) and fewer than
  ``MAX_ENQUEUES_PER_HEAD`` times, so one commit is retried at most
  ``MAX_ENQUEUES_PER_HEAD - 1`` times. The workflow passes that head to
  ``gh pr merge --match-head-commit``, so a push in between makes the
  enqueue fail instead of queueing the new commit.

For a ``pull_request`` run (``--rerun``) it re-runs the failed jobs when:

- the run is a failed ``pull_request`` run of this repository's CI
  workflow (a fork's pull request included: a re-run keeps the original
  run's read-only context), on attempt 1 to ``MAX_PR_RUN_ATTEMPTS - 1``,
  so one run gets at most ``MAX_PR_RUN_ATTEMPTS - 1`` re-runs;
- the jobs pass the same not-acquired test as above. A run cancelled by a
  newer push has no not-acquired annotation, so it is never re-run.

The checks are sans-IO functions over the bytes the GitHub API returns, so
``scripts/tests/test_merge_queue_requeue.py`` drives the same code the
workflow does. See ``docs/contributing/ci.md#merge-queue-re-enqueue``.

Usage: ``merge_queue_requeue.py --repo OWNER/NAME --run-id ID [--rerun]``
with ``GH_TOKEN`` set. Without ``--rerun`` it writes ``pr=<n>`` and
``sha=<head>`` to ``$GITHUB_OUTPUT`` (or stdout without it) when the pull
request should go back in the queue; with ``--rerun`` it writes
``rerun=true`` when the run's failed jobs should run again. It writes one
line to ``$GITHUB_STEP_SUMMARY`` either way.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Sequence

# The CI workflow whose merge-group runs are watched.
CI_WORKFLOW_PATH = ".github/workflows/ci.yml"

# How many times one pull request head may enter the queue: the first
# enqueue plus two retries.
MAX_ENQUEUES_PER_HEAD = 3

# How many attempts one pull request CI run may have: the first plus two
# re-runs for jobs that never got a runner.
MAX_PR_RUN_ATTEMPTS = 3

# Jobs that fail or end because another job did, never on their own.
FOLLOWER_JOBS = ("CI Gate", "merge-queue fail-fast")

# The phrase GitHub's annotation uses when no hosted runner took the job.
NOT_ACQUIRED = "was not acquired by Runner"

# The branch a merge group runs on: the pull request number and base commit.
QUEUE_BRANCH_RE = re.compile(r"gh-readonly-queue/main/pr-([1-9][0-9]{0,9})-[0-9a-f]{40}")

# A full commit id, as the API reports it.
SHA_RE = re.compile(r"[0-9a-f]{40}")

# Timeline events after which the pull request has a new head.
HEAD_CHANGE_EVENTS = frozenset({"committed", "head_ref_force_pushed"})

# How long one `gh` call may take before the job gives up.
GH_TIMEOUT_SECONDS = 120

# Runs `gh` with the given arguments and returns its stdout; raises
# `subprocess.CalledProcessError` when it fails.
Gh = Callable[[Sequence[str]], bytes]


def _load(raw: bytes) -> object:
    """Parses a JSON body, or returns None when it is not JSON.

    Args:
        raw: The body's bytes.

    Returns:
        The parsed value, or None.
    """
    try:
        return json.loads(raw)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return None


def _is_int(value: object) -> bool:
    """Whether a JSON value is an integer (and not a boolean).

    Args:
        value: The value.

    Returns:
        True for a non-boolean ``int``.
    """
    return isinstance(value, int) and not isinstance(value, bool)


def queued_pr(raw: bytes, repo: str) -> int | None:
    """Reads which pull request a failed merge-group CI run was testing.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}``.
        repo: ``OWNER/NAME`` of this repository.

    Returns:
        The pull request number when the run is a completed, failed or
        cancelled ``merge_group`` run of ``CI_WORKFLOW_PATH`` in ``repo`` on
        a queue branch; otherwise None.
    """
    run = _load(raw)
    if not isinstance(run, dict):
        return None
    head_repo = run.get("head_repository")
    branch = run.get("head_branch")
    if (
        run.get("event") != "merge_group"
        or run.get("path") != CI_WORKFLOW_PATH
        or run.get("status") != "completed"
        or run.get("conclusion") not in ("failure", "cancelled")
        or not isinstance(head_repo, dict)
        or head_repo.get("full_name") != repo
        or not isinstance(branch, str)
    ):
        return None
    match = QUEUE_BRANCH_RE.fullmatch(branch)
    return int(match.group(1)) if match else None


def pr_run_attempt(raw: bytes, repo: str) -> int | None:
    """Reads the attempt number of a failed pull request CI run.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}``.
        repo: ``OWNER/NAME`` of this repository.

    Returns:
        ``run_attempt`` (at least 1) when the run is a completed, failed
        ``pull_request`` run of ``CI_WORKFLOW_PATH`` in ``repo``; otherwise
        None. The head may come from a fork.
    """
    run = _load(raw)
    if not isinstance(run, dict):
        return None
    base = run.get("repository")
    attempt = run.get("run_attempt")
    if (
        run.get("event") != "pull_request"
        or run.get("path") != CI_WORKFLOW_PATH
        or run.get("status") != "completed"
        or run.get("conclusion") != "failure"
        or not isinstance(base, dict)
        or base.get("full_name") != repo
        or not _is_int(attempt)
        or attempt < 1
    ):
        return None
    return attempt


def _is_follower(name: str) -> bool:
    """Whether a job is one that only reports or reacts to other jobs.

    Args:
        name: The job's name; a called run prefixes it with ``<caller> / ``.

    Returns:
        True for ``CI Gate`` and ``merge-queue fail-fast``.
    """
    return any(name == job or name.endswith(" / " + job) for job in FOLLOWER_JOBS)


def stranded_jobs(raw: bytes) -> list[int] | None:
    """Finds the jobs that never got a runner, if nothing else went wrong.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}/jobs``.

    Returns:
        Ids of jobs that were cancelled without a runner or a single step,
        when every other job succeeded, was skipped or is a follower job
        (``FOLLOWER_JOBS``). None when any other job failed, was cancelled
        after it started, is still running, or the body is not the expected
        shape. An empty list means no job was stranded.
    """
    doc = _load(raw)
    jobs = doc.get("jobs") if isinstance(doc, dict) else None
    if not isinstance(jobs, list):
        return None
    stranded: list[int] = []
    for job in jobs:
        if not isinstance(job, dict):
            return None
        name = job.get("name")
        job_id = job.get("id")
        conclusion = job.get("conclusion")
        if not isinstance(name, str) or not _is_int(job_id):
            return None
        if conclusion in ("success", "skipped") or _is_follower(name):
            continue
        steps = job.get("steps")
        if conclusion == "cancelled" and not job.get("runner_name") and (steps is None or steps == []):
            stranded.append(job_id)
            continue
        return None
    return stranded


def not_acquired(raw: bytes) -> bool:
    """Whether a job's annotations say no hosted runner took it.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/check-runs/{id}/annotations``.

    Returns:
        True when some ``failure`` annotation's message contains
        ``NOT_ACQUIRED``.
    """
    doc = _load(raw)
    if not isinstance(doc, list):
        return False
    return any(
        isinstance(a, dict)
        and a.get("annotation_level") == "failure"
        and isinstance(a.get("message"), str)
        and NOT_ACQUIRED in a["message"]
        for a in doc
    )


def pr_head(raw: bytes, repo: str) -> str | None:
    """Reads a pull request that may be put back in the queue.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/pulls/{n}``.
        repo: ``OWNER/NAME`` of this repository.

    Returns:
        The head commit id when the pull request is open, not a draft, has
        no auto-merge set and its head is in ``repo``; otherwise None.
    """
    pr = _load(raw)
    if not isinstance(pr, dict):
        return None
    head = pr.get("head")
    if not isinstance(head, dict):
        return None
    head_repo = head.get("repo")
    sha = head.get("sha")
    if (
        pr.get("state") != "open"
        or pr.get("draft") is not False
        or pr.get("auto_merge") is not None
        or not isinstance(head_repo, dict)
        or head_repo.get("full_name") != repo
        or not isinstance(sha, str)
        or not SHA_RE.fullmatch(sha)
    ):
        return None
    return sha


def enqueues_since_head(raw: bytes) -> int | None:
    """Counts how often the pull request entered the queue on its head.

    Args:
        raw: The ``gh api --paginate --slurp`` body of
            ``GET /repos/{repo}/issues/{n}/timeline``: a list of pages, each
            a list of events, oldest first.

    Returns:
        The number of ``added_to_merge_queue`` events after the last event
        in ``HEAD_CHANGE_EVENTS``; None when the body is not the expected
        shape.
    """
    pages = _load(raw)
    if not isinstance(pages, list):
        return None
    count = 0
    for page in pages:
        if not isinstance(page, list):
            return None
        for event in page:
            if not isinstance(event, dict):
                return None
            kind = event.get("event")
            if isinstance(kind, str) and kind in HEAD_CHANGE_EVENTS:
                count = 0
            elif kind == "added_to_merge_queue":
                count += 1
    return count


def _stranded_reason(repo: str, run_id: int, gh: Gh) -> str | None:
    """Checks that a run failed only because jobs never got a runner.

    Args:
        repo: ``OWNER/NAME`` of this repository.
        run_id: The failed CI workflow run.
        gh: Runs ``gh``.

    Returns:
        None when every unsuccessful job is a follower job or a stranded
        job with GitHub's not-acquired annotation, and at least one is
        stranded; otherwise a sentence saying why not.

    Raises:
        subprocess.CalledProcessError: When a GitHub API call fails.
    """
    stranded = stranded_jobs(gh(["api", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"]))
    if stranded is None:
        return f"run {run_id} has a job that failed or was cancelled after it started."
    if not stranded:
        return f"run {run_id} has no job that waited for a runner."
    for job_id in stranded:
        if not not_acquired(gh(["api", f"repos/{repo}/check-runs/{job_id}/annotations"])):
            return f"job {job_id} of run {run_id} was cancelled without GitHub's not-acquired annotation."
    return None


def decide_rerun(repo: str, run_id: int, gh: Gh) -> tuple[bool, str]:
    """Decides whether a pull request CI run's failed jobs run again.

    Args:
        repo: ``OWNER/NAME`` of this repository.
        run_id: The failed CI workflow run.
        gh: Runs ``gh`` (``run_gh`` outside tests).

    Returns:
        ``(True, reason)`` when the failed jobs should run again, else
        ``(False, reason)``; ``reason`` is one sentence.
    """
    try:
        attempt = pr_run_attempt(gh(["api", f"repos/{repo}/actions/runs/{run_id}"]), repo)
        if attempt is None:
            return False, f"run {run_id} is not a failed pull request CI run of {repo}."
        if attempt >= MAX_PR_RUN_ATTEMPTS:
            return False, f"run {run_id} already ran {attempt} times."
        why_not = _stranded_reason(repo, run_id, gh)
        if why_not is not None:
            return False, why_not
    except subprocess.CalledProcessError:
        return False, "a GitHub API call failed."
    return True, f"run {run_id} failed only because GitHub never gave a job a runner; re-running its failed jobs."


def decide(repo: str, run_id: int, gh: Gh) -> tuple[tuple[int, str] | None, str]:
    """Decides whether the run's pull request goes back in the queue.

    Args:
        repo: ``OWNER/NAME`` of this repository.
        run_id: The failed CI workflow run.
        gh: Runs ``gh`` (``run_gh`` outside tests).

    Returns:
        ``((number, head), reason)`` with the pull request number and the
        head commit that was queued when it should be enqueued again, else
        ``(None, reason)``; ``reason`` is one sentence.
    """
    try:
        pr = queued_pr(gh(["api", f"repos/{repo}/actions/runs/{run_id}"]), repo)
        if pr is None:
            return None, f"run {run_id} is not a failed merge-group CI run of {repo}."
        why_not = _stranded_reason(repo, run_id, gh)
        if why_not is not None:
            return None, f"#{pr}: {why_not}"
        head = pr_head(gh(["api", f"repos/{repo}/pulls/{pr}"]), repo)
        if head is None:
            return None, f"#{pr} is closed, a draft, already has auto-merge, or comes from a fork."
        timeline = gh(["api", "--paginate", "--slurp", f"repos/{repo}/issues/{pr}/timeline?per_page=100"])
        enqueues = enqueues_since_head(timeline)
        if enqueues is None:
            return None, f"could not read the timeline of #{pr}."
        if enqueues == 0:
            return None, f"#{pr} has a new head since it was queued; it waits for its own enqueue."
        if enqueues >= MAX_ENQUEUES_PER_HEAD:
            return None, f"#{pr} already entered the queue {enqueues} times on its current head."
    except subprocess.CalledProcessError:
        return None, "a GitHub API call failed."
    return (pr, head), f"#{pr} left the queue only because GitHub never gave run {run_id} a runner; enqueueing it again."


def run_gh(args: Sequence[str]) -> bytes:
    """Runs ``gh`` and returns its stdout.

    Args:
        args: The arguments after ``gh``.

    Returns:
        What ``gh`` printed.

    Raises:
        subprocess.CalledProcessError: When ``gh`` exits non-zero or takes
            longer than ``GH_TIMEOUT_SECONDS``.
    """
    try:
        return subprocess.run(["gh", *args], check=True, capture_output=True, timeout=GH_TIMEOUT_SECONDS).stdout
    except subprocess.TimeoutExpired as err:
        raise subprocess.CalledProcessError(124, err.cmd) from err


def main(argv: Sequence[str] | None = None, gh: Gh = run_gh) -> int:
    """Command-line entry point.

    Args:
        argv: Arguments without the program name; ``sys.argv[1:]`` when None.
        gh: Runs ``gh``.

    Returns:
        0, also when nothing is recovered; 2 on bad arguments.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--rerun", action="store_true", help="decide on a pull request run's re-run")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo) or args.run_id <= 0:
        print("merge_queue_requeue: bad --repo or --run-id", file=sys.stderr)
        return 2
    if args.rerun:
        rerun, reason = decide_rerun(args.repo, args.run_id, gh)
        output = "rerun=true\n" if rerun else ""
        label = "Pull request re-run"
    else:
        target, reason = decide(args.repo, args.run_id, gh)
        output = f"pr={target[0]}\nsha={target[1]}\n" if target is not None else ""
        label = "Merge-queue re-enqueue"
    print(reason)
    if output:
        out = os.environ.get("GITHUB_OUTPUT")
        if out:
            with open(out, "a", encoding="utf-8") as fh:
                fh.write(output)
        else:
            print(output, end="")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(f"{label}: {reason}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
