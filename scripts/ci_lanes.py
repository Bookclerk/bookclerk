#!/usr/bin/env python3
"""Sends each heavy pull request CI job to the lane with the shortest line.

The organization has 20 hosted runners, and GitHub gives a ``merge_group``
run no priority over pull request runs. So pull request jobs wait in
**lanes**: job-level ``concurrency`` groups named ``ci-pr-lane-<n>`` with
``queue: max``, one running job per lane. The plan job runs this script on
a ``pull_request`` event to pick each of its jobs' lanes:

- Lanes ``0`` to ``BASE_LANES - 1`` are always open to pull requests.
  Together with CodeQL's and ClusterFuzzLite's own slots they make the 12
  runners pull requests may always hold, so the merge queue keeps at least
  8 whatever pull requests do.
- Lanes ``BASE_LANES`` to ``BASE_LANES + BORROWED_LANES - 1`` are lent to
  pull requests only while no ``merge_group`` CI run is active, and only
  when nothing runs or waits in them, so each normally holds one job. When
  a merge group arrives, no new job enters them and they empty within one
  pull request job, giving the queue its runners back. There is no lock:
  plans that read the jobs API within the same few seconds, before either
  run's jobs exist, can each place a job in the same empty lane, so a lent
  lane holds at most one job per plan in that window and empties within
  that many jobs.
- Each job goes to the open lane with the fewest jobs running or waiting
  in it, counted from the jobs of every active pull request CI run. A run's
  own jobs count as they are placed, so they spread out. On a tie a base
  lane wins over a lent one, so lent lanes fill only when every base lane
  is busy; ties within each kind are broken by a shuffle seeded with the
  run id, so two plans finishing at once rarely pick the same lane.

Lane loads come from job names: a lane job's name ends in
``LABEL_PREFIX<n>`` on a pull request (``unit + integration tests · lane
3``). When the API can't be read, every job goes to a base lane in the
seeded order with no lane lent, which is the limit pull requests always
had.

The readers are sans-IO functions over the bytes the GitHub API returns,
so ``scripts/tests/test_ci_lanes.py`` drives the same code the workflow
does. See ``docs/contributing/ci.md#pull-request-lanes``.

Usage: ``ci_lanes.py --repo OWNER/NAME --run-id ID --outputs JSON`` with
``GH_TOKEN`` set, where ``JSON`` is the plan's resolve step outputs
(``toJSON(steps.resolve.outputs)``). Writes ``lanes=<JSON>`` to
``$GITHUB_OUTPUT`` (or stdout without it): an object from
``<job>-<leg>`` to ``{"group": ..., "label": ...}``.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence

# The CI workflow whose runs share the lanes.
CI_WORKFLOW_FILE = "ci.yml"

# The path the API reports for that workflow.
CI_WORKFLOW_PATH = ".github/workflows/ci.yml"

# Lanes pull requests may always use. With CodeQL's two slots and
# ClusterFuzzLite's one they hold at most 12 of the 20 runners.
BASE_LANES = 9

# Lanes lent to pull requests while the merge queue is idle, one job each:
# with them pull requests can use all 20 runners.
BORROWED_LANES = 8

# Every lane.
ALL_LANES = BASE_LANES + BORROWED_LANES

# The concurrency group of lane ``n`` is this prefix followed by ``n``.
GROUP_PREFIX = "ci-pr-lane-"

# A lane job's name ends in this text followed by the lane number.
LABEL_PREFIX = " · lane "

# Reads the lane number at the end of a job name.
LABEL_RE = re.compile(re.escape(LABEL_PREFIX) + r"(0|[1-9][0-9]?)\Z")

# CI jobs that wait in lanes on a pull request, longest first so the
# longest jobs get the emptiest lanes.
LANE_JOBS = ("test", "release", "check", "e2e", "postgres", "confinement", "native-gateway", "desktop", "tray", "site")

# Jobs whose matrix has a fixed number of legs on a pull request. Jobs in
# ``MATRIX_OUTPUTS`` take theirs from a plan output; every other job has one.
FIXED_LEGS = {"postgres": 3}

# Jobs whose matrix is a JSON list in a plan output, one leg per entry: the
# confinement, native-gateway and tray jobs have one leg per runner label
# in ``os_matrix``, and the test job one per part in ``test_parts``.
MATRIX_OUTPUTS = {"confinement": "os_matrix", "native-gateway": "os_matrix", "tray": "os_matrix", "test": "test_parts"}

# Run statuses of a run that hasn't finished. A run whose jobs wait in a
# concurrency group is ``pending``.
ACTIVE_RUN_STATUSES = ("queued", "in_progress", "pending", "waiting", "requested")

# Job statuses of a job that holds or waits for a lane.
ACTIVE_JOB_STATUSES = frozenset({"queued", "in_progress", "pending", "waiting", "requested"})

# How many active pull request runs' jobs are read at most; a longer list
# is cut, so loads are a lower bound.
MAX_RUNS_READ = 40

# How long one `gh` call may take before the plan gives up on it.
GH_TIMEOUT_SECONDS = 30

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


def active_runs(raw: bytes, repo: str) -> tuple[bool, list[int]] | None:
    """Reads one page of CI runs: is a merge group active, and which pull request runs are.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/workflows/ci.yml/runs``.
        repo: ``OWNER/NAME`` of this repository.

    Returns:
        ``(merge group active, pull request run ids)`` counting only runs of
        ``CI_WORKFLOW_PATH`` in ``repo`` whose status is in
        ``ACTIVE_RUN_STATUSES``; None when the body is not a run listing or
        any entry is malformed, so the caller falls back.
    """
    doc = _load(raw)
    if not isinstance(doc, dict):
        return None
    runs = doc.get("workflow_runs")
    if not isinstance(runs, list):
        return None
    queue = False
    prs: list[int] = []
    for run in runs:
        if not isinstance(run, dict):
            return None
        run_id, event, status, path = run.get("id"), run.get("event"), run.get("status"), run.get("path")
        repository = run.get("repository")
        if not _is_int(run_id) or not isinstance(event, str) or not isinstance(status, str):
            return None
        if not isinstance(repository, dict) or repository.get("full_name") != repo or path != CI_WORKFLOW_PATH:
            continue
        if status not in ACTIVE_RUN_STATUSES:
            continue
        if event == "merge_group":
            queue = True
        elif event == "pull_request" and run_id > 0:
            prs.append(run_id)
    return queue, prs


def lane_loads(raw: bytes) -> dict[int, int] | None:
    """Counts the jobs of one run that run or wait in each lane.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/actions/runs/{id}/jobs``.

    Returns:
        A map from lane number (below ``ALL_LANES``) to the run's jobs whose
        status is in ``ACTIVE_JOB_STATUSES`` and whose name ends in that
        lane's label; None when the body is not a job listing or any entry
        is malformed.
    """
    doc = _load(raw)
    if not isinstance(doc, dict):
        return None
    jobs = doc.get("jobs")
    if not isinstance(jobs, list):
        return None
    loads: dict[int, int] = {}
    for job in jobs:
        if not isinstance(job, dict):
            return None
        name, status = job.get("name"), job.get("status")
        if not isinstance(name, str) or not isinstance(status, str):
            return None
        if status not in ACTIVE_JOB_STATUSES:
            continue
        match = LABEL_RE.search(name)
        if match is None:
            continue
        lane = int(match.group(1))
        if lane < ALL_LANES:
            loads[lane] = loads.get(lane, 0) + 1
    return loads


def planned_legs(outputs: Mapping[str, object]) -> list[str]:
    """Lists the ``<job>-<leg>`` keys of the lane jobs a plan runs.

    Args:
        outputs: The resolve step's outputs (``job_<job>`` is ``"true"``
            for a planned job; each output in ``MATRIX_OUTPUTS`` is a JSON
            list with one entry per leg).

    Returns:
        The keys, in ``LANE_JOBS`` order and then by leg. A matrix output
        that is missing, not a list or empty counts as one leg.
    """
    keys: list[str] = []
    for job in LANE_JOBS:
        if outputs.get(f"job_{job.replace('-', '_')}") != "true":
            continue
        legs = FIXED_LEGS.get(job, 1)
        if job in MATRIX_OUTPUTS:
            legs = 1
            raw = outputs.get(MATRIX_OUTPUTS[job])
            matrix = _load(raw.encode()) if isinstance(raw, str) else None
            if isinstance(matrix, list) and matrix:
                legs = len(matrix)
        keys.extend(f"{job}-{leg}" for leg in range(legs))
    return keys


def assign(keys: Sequence[str], loads: Mapping[int, int] | None, queue_active: bool, seed: int) -> dict[str, int]:
    """Picks a lane for each job leg: the open lane with the shortest line.

    Args:
        keys: The run's ``<job>-<leg>`` keys, in placement order.
        loads: Jobs running or waiting per lane across active pull request
            runs, or None when they couldn't be read (then no lane is lent
            and jobs go round the base lanes in seeded order).
        queue_active: Whether a ``merge_group`` CI run is active (then no
            lane is lent).
        seed: The run id, which seeds the tie-break shuffle.

    Returns:
        A map from each key to its lane number.
    """
    order = list(range(ALL_LANES))
    random.Random(seed).shuffle(order)
    rank = {lane: i for i, lane in enumerate(order)}
    count = {lane: (loads or {}).get(lane, 0) for lane in range(ALL_LANES)}
    lend = loads is not None and not queue_active
    picked: dict[str, int] = {}
    for key in keys:
        open_lanes = list(range(BASE_LANES))
        if lend:
            open_lanes += [lane for lane in range(BASE_LANES, ALL_LANES) if count[lane] == 0]
        lane = min(open_lanes, key=lambda n: (count[n], n >= BASE_LANES, rank[n]))
        count[lane] += 1
        picked[key] = lane
    return picked


def lanes_output(picked: Mapping[str, int]) -> str:
    """Renders the ``lanes`` plan output.

    Args:
        picked: The lane of each ``<job>-<leg>`` key.

    Returns:
        Compact ASCII JSON: each key maps to its concurrency ``group`` and
        the ``label`` its job name ends in.
    """
    return json.dumps(
        {key: {"group": f"{GROUP_PREFIX}{lane}", "label": f"{LABEL_PREFIX}{lane}"} for key, lane in picked.items()},
        separators=(",", ":"),
        sort_keys=True,
    )


def read_state(repo: str, own_run: int, gh: Gh) -> tuple[dict[int, int] | None, bool, str]:
    """Reads the lane loads and whether a merge group is active.

    Args:
        repo: ``OWNER/NAME`` of this repository.
        own_run: This run's id, whose jobs don't exist yet and are skipped.
        gh: Runs ``gh``.

    Returns:
        ``(loads, queue active, reason)``; loads are None when anything
        couldn't be read, and the queue then counts as active.
    """
    queue = False
    runs: list[int] = []
    for status in ACTIVE_RUN_STATUSES:
        try:
            raw = gh(["api", f"repos/{repo}/actions/workflows/{CI_WORKFLOW_FILE}/runs?status={status}&per_page=100"])
        except subprocess.CalledProcessError:
            return None, True, f"could not list {status} CI runs"
        listed = active_runs(raw, repo)
        if listed is None:
            return None, True, f"unreadable {status} CI run listing"
        queue = queue or listed[0]
        runs.extend(r for r in listed[1] if r != own_run and r not in runs)
    loads: dict[int, int] = {}
    for run_id in runs[:MAX_RUNS_READ]:
        try:
            raw = gh(["api", f"repos/{repo}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"])
        except subprocess.CalledProcessError:
            return None, True, f"could not list the jobs of run {run_id}"
        run_loads = lane_loads(raw)
        if run_loads is None:
            return None, True, f"unreadable job listing of run {run_id}"
        for lane, n in run_loads.items():
            loads[lane] = loads.get(lane, 0) + n
    busy = sum(loads.values())
    return loads, queue, f"{len(runs)} active pull request runs, {busy} lane jobs, merge queue {'busy' if queue else 'idle'}"


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
        0, also when the API couldn't be read (jobs then use base lanes);
        2 on bad arguments.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--outputs", required=True, help="the resolve step's outputs as JSON")
    args = parser.parse_args(argv)
    outputs = _load(args.outputs.encode())
    if (
        not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo)
        or args.run_id <= 0
        or not isinstance(outputs, dict)
    ):
        print("ci_lanes: bad --repo, --run-id or --outputs", file=sys.stderr)
        return 2
    keys = planned_legs(outputs)
    if keys:
        loads, queue, reason = read_state(args.repo, args.run_id, gh)
    else:
        loads, queue, reason = {}, False, "no lane jobs planned"
    picked = assign(keys, loads, queue, args.run_id)
    lent = sum(1 for lane in picked.values() if lane >= BASE_LANES)
    line = f"{reason}; {len(picked)} jobs placed, {lent} in lent lanes" + (
        "" if loads is not None else " (fallback: base lanes only)"
    )
    print(line)
    output = f"lanes={lanes_output(picked)}\n"
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write(output)
    else:
        print(output, end="")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(f"Pull request lanes: {line}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
