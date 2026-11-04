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
  newer push has no not-acquired annotation, so it is never re-run;
- no newer ``pull_request`` CI run exists for the same head branch and
  head repository. CI cancels superseded pull request runs in its
  concurrency group, so re-running an old run would cancel the run for
  the pull request's current commit.

For ``CI Gate`` itself (``--stranded-job NAME``), it decides whether a
job that runs other jobs' checks on one more OS (the macOS job) may pass
the gate although GitHub never gave it a runner, once that has already
been retried:

- the job ``NAME`` of the run's current attempt was cancelled without a
  runner or a step and carries the not-acquired annotation;
- for a ``pull_request`` run, this is attempt 2 or later and the same job
  was stranded the same way on the attempt before (the ``rerun`` job
  above re-ran it once);
- for a ``merge_group`` run, the pull request already entered the queue at
  least ``MIN_ENQUEUES_TO_PASS_STRANDED`` times on its current head and
  the same job was stranded the same way in its previous merge-group run
  (the re-enqueue above retried it once).

Any other run (a ``main`` push, an OSV remediation run) never passes a
stranded job.

The checks are sans-IO functions over the bytes the GitHub API returns, so
``scripts/tests/test_merge_queue_requeue.py`` drives the same code the
workflow does. See ``docs/contributing/ci.md#merge-queue-re-enqueue``.

Usage: ``merge_queue_requeue.py --repo OWNER/NAME --run-id ID [--rerun |
--stranded-job NAME --run-attempt N]`` with ``GH_TOKEN`` set. Without a
mode flag it writes ``pr=<n>`` and ``sha=<head>`` to ``$GITHUB_OUTPUT`` (or
stdout without it) when the pull request should go back in the queue; with
``--rerun`` it writes ``rerun=true`` when the run's failed jobs should run
again; with ``--stranded-job`` it writes ``stranded=true`` when that job
may pass ``CI Gate``. It writes one line to ``$GITHUB_STEP_SUMMARY`` either
way.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Sequence
from urllib.parse import quote

# The CI workflow whose merge-group runs are watched.
CI_WORKFLOW_PATH = ".github/workflows/ci.yml"

# How many times one pull request head may enter the queue: the first
# enqueue plus two retries.
MAX_ENQUEUES_PER_HEAD = 3

# How many attempts one pull request CI run may have: the first plus two
# re-runs for jobs that never got a runner.
MAX_PR_RUN_ATTEMPTS = 3

# How many times a pull request head must have entered the queue before a
# merge group may pass ``CI Gate`` with a stranded OS leg job: the first
# enqueue plus one retry.
MIN_ENQUEUES_TO_PASS_STRANDED = 2

# How many attempts a pull request CI run must have reached before it may
# pass ``CI Gate`` with a stranded OS leg job: the first plus one re-run.
MIN_PR_ATTEMPT_TO_PASS_STRANDED = 2

# Jobs that fail or end because another job did, never on their own.
FOLLOWER_JOBS = ("CI Gate", "merge-queue fail-fast")

# The phrase GitHub's annotation uses when no hosted runner took the job.
NOT_ACQUIRED = "was not acquired by Runner"

# The branch a merge group runs on: the pull request number and base commit.
QUEUE_BRANCH_RE = re.compile(r"gh-readonly-queue/main/pr-([1-9][0-9]{0,9})-[0-9a-f]{40}")

# A full commit id, as the API reports it.
SHA_RE = re.compile(r"[0-9a-f]{40}")

# A UTC timestamp as the API reports it; strings of this shape order by time.
TIME_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")

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


def gate_run(raw: bytes, repo: str) -> tuple[str, str, int] | None:
    """Reads the event and head branch of the CI run ``CI Gate`` belongs to.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}``.
        repo: ``OWNER/NAME`` of this repository.

    Returns:
        ``(event, head branch, run_number)`` when the run is a
        ``pull_request`` or ``merge_group`` run of ``CI_WORKFLOW_PATH`` in
        ``repo`` with a non-empty head branch and a ``run_number`` of at
        least 1; otherwise None. The run is still in progress, since
        ``CI Gate`` is one of its jobs.
    """
    run = _load(raw)
    if not isinstance(run, dict):
        return None
    base = run.get("repository")
    event = run.get("event")
    branch = run.get("head_branch")
    number = run.get("run_number")
    if (
        event not in ("pull_request", "merge_group")
        or run.get("path") != CI_WORKFLOW_PATH
        or not isinstance(base, dict)
        or base.get("full_name") != repo
        or not isinstance(branch, str)
        or not branch
        or not _is_int(number)
        or number < 1
    ):
        return None
    return event, branch, number


def stranded_job(raw: bytes, name: str) -> int | None:
    """Finds one job by name and returns its id if it never got a runner.

    Args:
        raw: The JSON body of
            ``GET /repos/{repo}/actions/runs/{id}/attempts/{n}/jobs``.
        name: The job's name; a called run prefixes it with ``<caller> / ``.

    Returns:
        The id of the only job named ``name`` when it was cancelled without
        a runner or a single step; None when there is no such job, more
        than one, it ran, or the body is not the expected shape.
    """
    doc = _load(raw)
    jobs = doc.get("jobs") if isinstance(doc, dict) else None
    if not isinstance(jobs, list):
        return None
    found: list[dict] = []
    for job in jobs:
        if not isinstance(job, dict):
            return None
        job_name = job.get("name")
        if not isinstance(job_name, str):
            return None
        if _named(job_name, name):
            found.append(job)
    if len(found) != 1:
        return None
    job = found[0]
    job_id = job.get("id")
    if not _is_int(job_id) or job.get("status") != "completed" or not _never_ran(job):
        return None
    return job_id


def previous_queue_run(raw: bytes, pr: int, run_number: int, since: str) -> int | None:
    """Finds the latest earlier merge-group CI run for one pull request head.

    Args:
        raw: The ``gh api --paginate --slurp`` body of
            ``GET /repos/{repo}/actions/workflows/{file}/runs`` filtered to
            ``merge_group`` runs: a list of pages, each a run listing.
        pr: The pull request the current merge group tests.
        run_number: The current run's ``run_number``.
        since: When the pull request first entered the queue on its current
            head (``first_enqueue_on_head``); earlier runs tested an older head.

    Returns:
        The id of the run with the largest ``run_number`` below
        ``run_number`` whose head branch is a queue branch for ``pr`` and
        which was created at or after ``since``; None when there is none,
        ``since`` is not a timestamp, or the body is not a run listing.
    """
    pages = _load(raw)
    if not isinstance(pages, list) or not TIME_RE.fullmatch(since):
        return None
    best: tuple[int, int] | None = None
    for page in pages:
        runs = page.get("workflow_runs") if isinstance(page, dict) else None
        if not isinstance(runs, list):
            return None
        for run in runs:
            if not isinstance(run, dict):
                return None
            number = run.get("run_number")
            run_id = run.get("id")
            branch = run.get("head_branch")
            created = run.get("created_at")
            if not _is_int(number) or not _is_int(run_id) or not isinstance(branch, str):
                return None
            if not isinstance(created, str) or not TIME_RE.fullmatch(created):
                return None
            match = QUEUE_BRANCH_RE.fullmatch(branch)
            if match is None or int(match.group(1)) != pr or number >= run_number or created < since:
                continue
            if best is None or number > best[0]:
                best = (number, run_id)
    return best[1] if best is not None else None


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


def run_lineage(raw: bytes) -> tuple[str, str, int] | None:
    """Reads where a CI run's head comes from and its place in the workflow.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}``.

    Returns:
        ``(head repository, head branch, run_number)`` with a non-empty
        ``OWNER/NAME`` and branch and a ``run_number`` of at least 1;
        otherwise None. ``run_number`` grows with every new run of a
        workflow and stays the same across re-runs.
    """
    run = _load(raw)
    if not isinstance(run, dict):
        return None
    head_repo = run.get("head_repository")
    branch = run.get("head_branch")
    number = run.get("run_number")
    if not isinstance(head_repo, dict):
        return None
    name = head_repo.get("full_name")
    if not isinstance(name, str) or not name or not isinstance(branch, str) or not branch:
        return None
    if not _is_int(number) or number < 1:
        return None
    return name, branch, number


def newer_run(raw: bytes, head_repo: str, run_number: int) -> bool | None:
    """Whether a run listing holds a later run from the same head repository.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/workflows/{file}/runs``
            filtered to one event and head branch.
        head_repo: ``OWNER/NAME`` the head branch lives in (a fork's
            branch can share its name with one in this repository).
        run_number: The ``run_number`` of the run being recovered.

    Returns:
        True when a run from ``head_repo`` has a larger ``run_number``,
        False when none has, and None when the body is not a run listing.
    """
    body = _load(raw)
    if not isinstance(body, dict):
        return None
    runs = body.get("workflow_runs")
    if not isinstance(runs, list):
        return None
    newer = False
    for run in runs:
        if not isinstance(run, dict):
            return None
        repo = run.get("head_repository")
        number = run.get("run_number")
        if not isinstance(repo, dict) or not _is_int(number):
            return None
        if repo.get("full_name") == head_repo and number > run_number:
            newer = True
    return newer


def _named(job_name: str, name: str) -> bool:
    """Whether a job's name is ``name``, also behind a caller's prefix.

    Args:
        job_name: The name the jobs listing shows; a called run prefixes it
            with ``<caller> / ``.
        name: The job's own name.

    Returns:
        True for ``name`` and ``<caller> / name``.
    """
    return job_name == name or job_name.endswith(" / " + name)


def _never_ran(job: dict) -> bool:
    """Whether a listed job was cancelled before it got a runner.

    Args:
        job: One entry of a jobs listing.

    Returns:
        True when it concluded ``cancelled`` with no runner and no steps.
    """
    steps = job.get("steps")
    return job.get("conclusion") == "cancelled" and not job.get("runner_name") and (steps is None or steps == [])


def _is_follower(name: str) -> bool:
    """Whether a job is one that only reports or reacts to other jobs.

    Args:
        name: The job's name; a called run prefixes it with ``<caller> / ``.

    Returns:
        True for ``CI Gate`` and ``merge-queue fail-fast``.
    """
    return any(_named(name, job) for job in FOLLOWER_JOBS)


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
        if _never_ran(job):
            stranded.append(job_id)
            continue
        return None
    return stranded


def gate_passed(raw: bytes) -> bool:
    """Whether a run's ``CI Gate`` job succeeded.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}/jobs``.

    Returns:
        True when some job named ``CI Gate`` (or ``<caller> / CI Gate``)
        concluded ``success``; False otherwise, also for a body that is not
        a job listing.
    """
    doc = _load(raw)
    jobs = doc.get("jobs") if isinstance(doc, dict) else None
    if not isinstance(jobs, list):
        return False
    return any(
        isinstance(job, dict)
        and isinstance(job.get("name"), str)
        and _named(job["name"], "CI Gate")
        and job.get("conclusion") == "success"
        for job in jobs
    )


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


def first_enqueue_on_head(raw: bytes) -> str | None:
    """Finds when the pull request first entered the queue on its head.

    Args:
        raw: The ``gh api --paginate --slurp`` body of
            ``GET /repos/{repo}/issues/{n}/timeline``: a list of pages, each
            a list of events, oldest first.

    Returns:
        The ``created_at`` of the first ``added_to_merge_queue`` event after
        the last event in ``HEAD_CHANGE_EVENTS``; None when there is none,
        that event has no timestamp, or the body is not the expected shape.
    """
    pages = _load(raw)
    if not isinstance(pages, list):
        return None
    first: object = None
    for page in pages:
        if not isinstance(page, list):
            return None
        for event in page:
            if not isinstance(event, dict):
                return None
            kind = event.get("event")
            if isinstance(kind, str) and kind in HEAD_CHANGE_EVENTS:
                first = None
            elif kind == "added_to_merge_queue" and first is None:
                first = event.get("created_at", "")
    return first if isinstance(first, str) and TIME_RE.fullmatch(first) else None


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
    jobs = gh(["api", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"])
    if gate_passed(jobs):
        return f"run {run_id} passed CI Gate; a stranded job there is one CI Gate let through."
    stranded = stranded_jobs(jobs)
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
        raw = gh(["api", f"repos/{repo}/actions/runs/{run_id}"])
        attempt = pr_run_attempt(raw, repo)
        lineage = run_lineage(raw)
        if attempt is None or lineage is None:
            return False, f"run {run_id} is not a failed pull request CI run of {repo}."
        if attempt >= MAX_PR_RUN_ATTEMPTS:
            return False, f"run {run_id} already ran {attempt} times."
        why_not = _stranded_reason(repo, run_id, gh)
        if why_not is not None:
            return False, why_not
        # Checked last, as close to the re-run as possible: re-running a
        # superseded run would cancel CI for the current commit.
        head_repo, branch, number = lineage
        workflow = CI_WORKFLOW_PATH.rsplit("/", 1)[-1]
        listing = gh(
            [
                "api",
                f"repos/{repo}/actions/workflows/{workflow}/runs"
                f"?event=pull_request&branch={quote(branch, safe='')}&per_page=100",
            ]
        )
        newer = newer_run(listing, head_repo, number)
        if newer is None:
            return False, f"could not list the CI runs of {head_repo}:{branch}."
        if newer:
            return False, f"run {run_id} is superseded by a newer run for {head_repo}:{branch}."
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


def _stranded_with_annotation(repo: str, jobs_url: str, name: str, gh: Gh) -> int | None:
    """Reads a jobs listing and returns the job ``name`` if it never got a runner.

    Args:
        repo: ``OWNER/NAME`` of this repository.
        jobs_url: The jobs listing to read, relative to the API root.
        name: The job's name.
        gh: Runs ``gh``.

    Returns:
        The job's id when it was cancelled with no runner and no step and
        carries GitHub's not-acquired annotation; otherwise None.

    Raises:
        subprocess.CalledProcessError: When a GitHub API call fails.
    """
    job_id = stranded_job(gh(["api", jobs_url]), name)
    if job_id is None:
        return None
    if not not_acquired(gh(["api", f"repos/{repo}/check-runs/{job_id}/annotations"])):
        return None
    return job_id


def decide_stranded(repo: str, run_id: int, attempt: int, name: str, gh: Gh) -> tuple[bool, str]:
    """Decides whether a stranded OS leg job may pass ``CI Gate``.

    Args:
        repo: ``OWNER/NAME`` of this repository.
        run_id: The CI run ``CI Gate`` is part of.
        attempt: That run's current attempt (``github.run_attempt``).
        name: The job's name, as the jobs listing shows it.
        gh: Runs ``gh`` (``run_gh`` outside tests).

    Returns:
        ``(True, reason)`` when the job never got a runner now and also
        never got one on the try before (the previous attempt of a pull
        request run, or the pull request's previous merge group on the
        same head), else ``(False, reason)``; ``reason`` is one sentence.
    """
    if attempt < 1:
        return False, f"run {run_id} has no attempt {attempt}."
    try:
        context = gate_run(gh(["api", f"repos/{repo}/actions/runs/{run_id}"]), repo)
        if context is None:
            return False, f"run {run_id} is not a pull request or merge-group CI run of {repo}."
        event, branch, number = context
        now = f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"
        if _stranded_with_annotation(repo, now, name, gh) is None:
            return False, f"{name} in run {run_id} ran, is missing, or was cancelled without GitHub's not-acquired annotation."
        if event == "pull_request":
            if attempt < MIN_PR_ATTEMPT_TO_PASS_STRANDED:
                return False, f"{name} never got a runner on attempt {attempt}; it is re-run once first."
            before = f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt - 1}/jobs?per_page=100"
            if _stranded_with_annotation(repo, before, name, gh) is None:
                return False, f"{name} got a runner on attempt {attempt - 1}; this is its first miss, so it is re-run once first."
        else:
            match = QUEUE_BRANCH_RE.fullmatch(branch)
            if match is None:
                return False, f"run {run_id} is not on a merge-queue branch."
            pr = int(match.group(1))
            timeline = gh(["api", "--paginate", "--slurp", f"repos/{repo}/issues/{pr}/timeline?per_page=100"])
            enqueues = enqueues_since_head(timeline)
            if enqueues is None:
                return False, f"could not read the timeline of #{pr}."
            if enqueues < MIN_ENQUEUES_TO_PASS_STRANDED:
                return False, f"{name} never got a runner on #{pr}'s first merge group; it is re-enqueued once first."
            since = first_enqueue_on_head(timeline)
            if since is None:
                return False, f"could not read when #{pr} entered the queue on its head."
            workflow = CI_WORKFLOW_PATH.rsplit("/", 1)[-1]
            runs = f"repos/{repo}/actions/workflows/{workflow}/runs?event=merge_group&created={quote('>=' + since, safe='')}&per_page=100"
            previous = previous_queue_run(gh(["api", "--paginate", "--slurp", runs]), pr, number, since)
            if previous is None:
                return False, f"found no earlier merge group of #{pr} on its head."
            before = f"repos/{repo}/actions/runs/{previous}/jobs?per_page=100"
            if _stranded_with_annotation(repo, before, name, gh) is None:
                return False, f"{name} got a runner in #{pr}'s previous merge group; this is its first miss there."
    except subprocess.CalledProcessError:
        return False, "a GitHub API call failed."
    return True, f"{name} never got a runner again after a retry; CI Gate passes without its checks."


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
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--rerun", action="store_true", help="decide on a pull request run's re-run")
    mode.add_argument("--stranded-job", help="decide whether this stranded job may pass CI Gate")
    parser.add_argument("--run-attempt", type=int, default=0, help="the run's attempt (with --stranded-job)")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo) or args.run_id <= 0:
        print("merge_queue_requeue: bad --repo or --run-id", file=sys.stderr)
        return 2
    if args.stranded_job is None and args.run_attempt:
        print("merge_queue_requeue: --run-attempt goes with --stranded-job", file=sys.stderr)
        return 2
    if args.stranded_job is not None:
        if not args.stranded_job or args.run_attempt <= 0:
            print("merge_queue_requeue: --stranded-job needs a name and --run-attempt", file=sys.stderr)
            return 2
        passes, reason = decide_stranded(args.repo, args.run_id, args.run_attempt, args.stranded_job, gh)
        output = "stranded=true\n" if passes else ""
        label = "Stranded job"
    elif args.rerun:
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
