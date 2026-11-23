#!/usr/bin/env python3
"""Carries a pull request's results over a visual approval commit.

Approving a pull request's screenshots makes the `Visual baselines` bot
commit `ui/visual/baselines.lock` to the pull request's branch
(``scripts/visual_baselines_bot.py approve``). The required checks attach to
that new head, so without this script every workflow would run again on
code it has already tested. When the new head is such a commit, the results
of the commit before it are carried over instead, as if the approval had
been recorded without a push. See
``docs/contributing/ci.md#visual-approval-commits``.

A head is carried over only when all of this holds; anything else prints why
and writes no output, and the workflow runs in full:

- The head commit has exactly one parent and changes exactly one file,
  ``ui/visual/baselines.lock``, in place (no rename, add or delete). Who
  made the commit does not matter: every other file is the parent's, byte
  for byte, so its results are the parent's.
- The newest run of the same workflow for the parent commit, from the same
  head repository and branch and started by ``pull_request``, finished.
- ``--kind run``: that run concluded ``success``.
- ``--kind ci`` (CI's plan job): that run kept its plan and its `CI Gate`
  verdict (``verdict`` below) for its latest attempt; the plan is a ready
  (not draft) pull request plan for the parent commit; and replaying the
  gate over the verdict, with the `ui-visual` job taken as passing, finds
  no problem, with the coverage and Web API contract steps successful. The
  visual job is the one thing the lock changes, and the carried run runs it
  again.

The merge queue runs the full suite again before anything reaches ``main``,
so a carried result never lands a commit by itself.

The selection is sans-IO over the bytes the GitHub API and the artifacts
hold, so ``scripts/tests/test_ci_carry.py`` drives the same code the
workflows do.

Usage:
  ``ci_carry.py find --repo R --workflow FILE --head SHA --head-repo R
  --head-branch BRANCH --kind ci|run [--dest DIR] [--carry-out FILE]``
  writes ``run-id=`` and ``parent=`` to ``$GITHUB_OUTPUT`` when the head is
  carried over (``--carry-out`` also writes them as JSON for
  ``ci-exec.py resolve --carried-from``).

  ``ci_carry.py verdict --out FILE`` (`CI Gate`) writes this run's verdict
  from ``NEEDS_JSON``, ``CI_GATE_STRANDED``, ``COVERAGE_OUTCOME``,
  ``CONTRACT_OUTCOME``, ``GITHUB_RUN_ID`` and ``GITHUB_RUN_ATTEMPT``.

  ``ci_carry.py sarif --repo R --run-id ID --language L --dest DIR`` (the
  CodeQL analyze job) downloads that run's kept SARIF for one language and
  writes ``sarif=<path>`` when it is a CodeQL analysis of that language.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

_SCRIPTS = Path(__file__).resolve().parent
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))

from ci_plan.execute import VISUAL_LOCK_PATH, ExecError, expected_jobs, gate, validate  # noqa: E402
from codeql_reuse import sarif_problem  # noqa: E402

# The one file an approval commit changes.
LOCK_PATH = VISUAL_LOCK_PATH

# The CI job that the lock change runs again; the carried run's own result
# for it is the one that counts.
VISUAL_JOB = "ui-visual"

# The artifact CI's plan job keeps, and the prefix of the one `CI Gate`
# keeps per attempt (`ci-gate-verdict-<attempt>`).
PLAN_ARTIFACT = "ci-plan"
VERDICT_ARTIFACT_PREFIX = "ci-gate-verdict-"

# The artifact each CodeQL analyze job keeps on a pull request run.
SARIF_ARTIFACT_PREFIX = "codeql-sarif-"

# The CodeQL languages codeql.yml analyzes on every run.
CODEQL_LANGUAGES = frozenset({"actions", "javascript-typescript", "python", "rust"})

# A full commit id, as `git rev-parse` prints it.
SHA_RE = re.compile(r"[0-9a-f]{40}")

# `owner/name` and a branch name as GitHub reports them; anything else is
# not looked up.
REPO_RE = re.compile(r"[A-Za-z0-9_.-]{1,100}/[A-Za-z0-9_.-]{1,100}")
BRANCH_RE = re.compile(r"[A-Za-z0-9._/:+-]{1,250}")

# Workflow files whose runs may be carried over.
WORKFLOW_RE = re.compile(r"[a-z0-9-]{1,60}\.yml")

# Step outcomes the verdict may record.
OUTCOMES = frozenset({"success", "failure", "cancelled", "skipped"})

# How long one `gh` call may take before the workflow gives up and runs in
# full.
GH_TIMEOUT_SECONDS = 120

# Runs `gh` with the given arguments and returns its stdout; raises
# `subprocess.CalledProcessError` when it fails.
Gh = Callable[[Sequence[str]], bytes]


@dataclass(frozen=True)
class ParentRun:
    """The run whose results a carried head reuses.

    Attributes:
        run_id: The workflow run's id.
        run_attempt: Its latest attempt, which the run's status and
            conclusion describe.
        status: The run's status, ``completed`` once it finished.
        conclusion: The run's conclusion, empty while it runs.
    """

    run_id: int
    run_attempt: int
    status: str
    conclusion: str


def _json(raw: bytes) -> Any:
    """Parses JSON, or returns None for bytes that are not JSON text."""
    try:
        return json.loads(raw)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return None


def _int(value: object) -> int | None:
    """The value when it is a positive integer (not a boolean), else None."""
    if isinstance(value, int) and not isinstance(value, bool) and value > 0:
        return value
    return None


def lock_only_parent(raw: bytes) -> str | None:
    """The parent of an approval commit.

    Args:
        raw: The JSON body of ``GET /repos/{repo}/commits/{sha}``.

    Returns:
        The full id of the commit's only parent when the commit changes
        exactly ``LOCK_PATH`` in place; None for any other commit or body.
    """
    doc = _json(raw)
    if not isinstance(doc, dict):
        return None
    parents = doc.get("parents")
    files = doc.get("files")
    if not isinstance(parents, list) or len(parents) != 1 or not isinstance(files, list) or len(files) != 1:
        return None
    parent = parents[0].get("sha") if isinstance(parents[0], dict) else None
    change = files[0]
    if not isinstance(parent, str) or SHA_RE.fullmatch(parent) is None or not isinstance(change, dict):
        return None
    if change.get("filename") != LOCK_PATH or change.get("status") != "modified" or "previous_filename" in change:
        return None
    return parent


def newest_run(raw: bytes, parent: str, head_repo: str, head_branch: str) -> ParentRun | None:
    """The newest pull request run of a workflow for the parent commit.

    Args:
        raw: The JSON body of
            ``GET /repos/{repo}/actions/workflows/{file}/runs``.
        parent: The parent commit's full id.
        head_repo: The pull request's head repository, ``owner/name``.
        head_branch: The pull request's head branch.

    Returns:
        The newest run started by ``pull_request`` for exactly ``parent``
        from that repository and branch, finished or not; None when there
        is none or the body is not the expected shape.
    """
    doc = _json(raw)
    runs = doc.get("workflow_runs") if isinstance(doc, dict) else None
    if not isinstance(runs, list):
        return None
    picked: list[tuple[str, int, ParentRun]] = []
    for run in runs:
        if not isinstance(run, dict):
            continue
        repo = run.get("head_repository")
        run_id = _int(run.get("id"))
        attempt = _int(run.get("run_attempt"))
        if (
            run_id is not None
            and attempt is not None
            and run.get("head_sha") == parent
            and run.get("event") == "pull_request"
            and run.get("head_branch") == head_branch
            and isinstance(repo, dict)
            and repo.get("full_name") == head_repo
            and isinstance(run.get("status"), str)
        ):
            conclusion = run.get("conclusion")
            picked.append(
                (
                    str(run.get("created_at") or ""),
                    run_id,
                    ParentRun(run_id, attempt, run["status"], conclusion if isinstance(conclusion, str) else ""),
                )
            )
    if not picked:
        return None
    picked.sort(key=lambda item: (item[0], item[1]), reverse=True)
    return picked[0][2]


def ci_verdict_problem(plan: Any, verdict: Any, run: ParentRun, parent: str) -> str | None:
    """Checks that a CI run passed everything but its visual job.

    Args:
        plan: The run's parsed ``ci-plan.json``.
        verdict: The run's parsed `CI Gate` verdict (``verdict`` below).
        run: The run.
        parent: The commit the run tested.

    Returns:
        None when the run's plan is a ready pull request plan for ``parent``
        and its gate, replayed with the visual job passing, finds nothing;
        otherwise a clause saying why the run can't be carried over.
    """
    if not isinstance(plan, dict):
        return "its plan is not a JSON object"
    try:
        validate(plan, run_id=str(run.run_id), checkout_sha=None)
    except ExecError as exc:
        return f"its plan is not valid here ({exc})"
    if plan.get("event") != "pull_request" or plan.get("head_sha") != parent:
        return "its plan is not a pull request plan for the parent commit"
    if plan["draft"] or plan["build_only"]:
        return "it ran a draft plan, which defers most jobs"
    if not isinstance(verdict, dict):
        return "its gate verdict is not a JSON object"
    if verdict.get("run_id") != run.run_id or verdict.get("run_attempt") != run.run_attempt:
        return "its gate verdict is from another run or attempt"
    for step in ("coverage", "contract"):
        outcome = verdict.get(step)
        if not isinstance(outcome, str) or outcome not in OUTCOMES:
            return f"its gate verdict has no {step} outcome"
        if outcome != "success":
            return f"its {step} step ended {outcome}"
    needs = verdict.get("needs")
    stranded = verdict.get("stranded")
    if not isinstance(needs, dict) or not all(
        isinstance(v, dict) and isinstance(v.get("result"), str) for v in needs.values()
    ):
        return "its gate verdict has no job results"
    if not isinstance(stranded, list) or not all(isinstance(s, str) for s in stranded):
        return "its gate verdict has no stranded list"
    wanted = expected_jobs(plan).get(VISUAL_JOB)
    if wanted is None:
        return "its plan does not know the visual job"
    replay = {job: dict(result) for job, result in needs.items()}
    replay[VISUAL_JOB] = {"result": "success" if wanted else "skipped"}
    problems = gate(plan, replay, frozenset(stranded))
    if problems:
        return "its gate found " + "; ".join(problems)
    return None


def _gh_ok(gh: Gh, args: Sequence[str]) -> bytes | None:
    """Runs ``gh`` and returns its stdout, or None when it failed."""
    try:
        return gh(args)
    except subprocess.CalledProcessError:
        return None


def _download_json(gh: Gh, repo: str, run_id: int, name: str, dest: Path) -> Any:
    """Downloads a one-file JSON artifact and parses it, or returns None."""
    target = dest / name
    if _gh_ok(gh, ["run", "download", str(run_id), "-R", repo, "-n", name, "-D", str(target)]) is None:
        return None
    files = sorted(p for p in target.iterdir() if p.is_file()) if target.is_dir() else []
    if len(files) != 1:
        return None
    try:
        return _json(files[0].read_bytes())
    except OSError:
        return None


def find(
    *,
    repo: str,
    workflow: str,
    head: str,
    head_repo: str,
    head_branch: str,
    kind: str,
    dest: Path,
    gh: Gh,
) -> tuple[ParentRun | None, str, str]:
    """Finds the parent run whose results the head carries over.

    Args:
        repo: The repository the runs belong to, ``owner/name``.
        workflow: The workflow file, such as ``ci.yml``.
        head: The pull request's head commit.
        head_repo: The pull request's head repository.
        head_branch: The pull request's head branch.
        kind: ``ci`` to check CI's plan and gate verdict, ``run`` to require
            a successful run.
        dest: An empty directory for downloads.
        gh: Runs the GitHub CLI.

    Returns:
        The parent run, the parent commit and a reason; or None, an empty
        string and why the head runs in full.
    """
    for value, pattern, what in (
        (repo, REPO_RE, "repository"),
        (head_repo, REPO_RE, "head repository"),
        (head_branch, BRANCH_RE, "head branch"),
        (head, SHA_RE, "head commit"),
        (workflow, WORKFLOW_RE, "workflow file"),
    ):
        if pattern.fullmatch(value) is None:
            return None, "", f"the {what} {value!r} is not one this script looks up"
    if kind not in ("ci", "run"):
        return None, "", f"unknown kind {kind!r}"
    commit = _gh_ok(gh, ["api", f"repos/{repo}/commits/{head}"])
    if commit is None:
        return None, "", "reading the head commit failed"
    parent = lock_only_parent(commit)
    if parent is None:
        return None, "", f"the head commit {head[:12]} is not an approval commit (one parent, only {LOCK_PATH} changed)"
    runs = _gh_ok(
        gh,
        [
            "api",
            "-X",
            "GET",
            f"repos/{repo}/actions/workflows/{workflow}/runs",
            "-f",
            f"head_sha={parent}",
            "-f",
            "event=pull_request",
            "-f",
            "per_page=100",
        ],
    )
    if runs is None:
        return None, "", f"listing the {workflow} runs failed"
    run = newest_run(runs, parent, head_repo, head_branch)
    if run is None:
        return None, "", f"{workflow} has no pull request run for the parent commit {parent[:12]}"
    if run.status != "completed":
        return None, "", f"{workflow} run {run.run_id} for the parent commit has not finished"
    if kind == "run":
        if run.conclusion != "success":
            return None, "", f"{workflow} run {run.run_id} for the parent commit ended {run.conclusion or 'without a conclusion'}"
        return run, parent, f"carried over {workflow} run {run.run_id} for {parent[:12]}"
    plan = _download_json(gh, repo, run.run_id, PLAN_ARTIFACT, dest)
    verdict = _download_json(gh, repo, run.run_id, f"{VERDICT_ARTIFACT_PREFIX}{run.run_attempt}", dest)
    if plan is None or verdict is None:
        return None, "", f"CI run {run.run_id} for the parent commit kept no plan or gate verdict"
    problem = ci_verdict_problem(plan, verdict, run, parent)
    if problem is not None:
        return None, "", f"CI run {run.run_id} can't be carried over: {problem}"
    return run, parent, f"carried over CI run {run.run_id} for {parent[:12]}"


def make_verdict(env: Mapping[str, str]) -> dict[str, Any]:
    """This `CI Gate` run's verdict, for a later approval commit to replay.

    Args:
        env: The gate's environment: ``NEEDS_JSON`` (``toJSON(needs)``),
            ``CI_GATE_STRANDED`` (comma-separated), ``COVERAGE_OUTCOME``,
            ``CONTRACT_OUTCOME``, ``GITHUB_RUN_ID`` and
            ``GITHUB_RUN_ATTEMPT``.

    Returns:
        The verdict: run id and attempt, each needed job's result, the
        stranded jobs and the two step outcomes (an unknown outcome is
        recorded as ``failure``).

    Raises:
        ExecError: The run id, attempt or needs are missing or malformed.
    """
    run_id = _int(int(env["GITHUB_RUN_ID"])) if env.get("GITHUB_RUN_ID", "").isdigit() else None
    attempt = _int(int(env["GITHUB_RUN_ATTEMPT"])) if env.get("GITHUB_RUN_ATTEMPT", "").isdigit() else None
    if run_id is None or attempt is None:
        raise ExecError("verdict needs GITHUB_RUN_ID and GITHUB_RUN_ATTEMPT")
    needs = _json(env.get("NEEDS_JSON", "").encode())
    if not isinstance(needs, dict):
        raise ExecError("verdict needs NEEDS_JSON (toJSON(needs))")
    results = {
        str(job): {"result": str(value.get("result"))}
        for job, value in needs.items()
        if isinstance(value, dict)
    }

    def outcome(name: str) -> str:
        value = env.get(name, "")
        return value if value in OUTCOMES else "failure"

    return {
        "run_id": run_id,
        "run_attempt": attempt,
        "needs": results,
        "stranded": sorted(s for s in env.get("CI_GATE_STRANDED", "").split(",") if s),
        "coverage": outcome("COVERAGE_OUTCOME"),
        "contract": outcome("CONTRACT_OUTCOME"),
    }


def kept_sarif(repo: str, run_id: str, language: str, dest: Path, gh: Gh) -> tuple[Path | None, str]:
    """Downloads a parent CodeQL run's SARIF for one language.

    Args:
        repo: ``owner/name``.
        run_id: The parent run ``find --kind run`` picked.
        language: The matrix language.
        dest: An empty directory to download into.
        gh: Runs the GitHub CLI.

    Returns:
        The SARIF file and a reason, or None and why the job analyzes.
    """
    if language not in CODEQL_LANGUAGES:
        return None, f"unknown language {language!r}"
    if not run_id.isdigit():
        return None, f"the run id {run_id!r} is not a number"
    target = dest / language
    if _gh_ok(gh, ["run", "download", run_id, "-R", repo, "-n", f"{SARIF_ARTIFACT_PREFIX}{language}", "-D", str(target)]) is None:
        return None, f"run {run_id} kept no {language} results"
    files = sorted(target.glob("*.sarif")) if target.is_dir() else []
    if len(files) != 1:
        return None, f"run {run_id} kept {len(files)} {language} SARIF files, not one"
    problem = sarif_problem(files[0].read_bytes(), f"/language:{language}")
    if problem is not None:
        return None, f"run {run_id}: {problem}"
    return files[0], f"reused the {language} results of run {run_id}"


def run_gh(args: Sequence[str]) -> bytes:
    """Runs the GitHub CLI and returns its stdout.

    Args:
        args: Arguments after ``gh``.

    Returns:
        The command's standard output.

    Raises:
        subprocess.CalledProcessError: When ``gh`` exits non-zero or takes
            longer than ``GH_TIMEOUT_SECONDS``, so a stalled call runs the
            workflow in full instead of holding the job.
    """
    try:
        return subprocess.run(["gh", *args], check=True, capture_output=True, timeout=GH_TIMEOUT_SECONDS).stdout
    except subprocess.TimeoutExpired as err:
        raise subprocess.CalledProcessError(124, err.cmd) from err


def _append(env_name: str, text: str) -> None:
    """Appends to the file a GitHub Actions variable names, or stdout for outputs."""
    path = os.environ.get(env_name)
    if path:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(text)
    elif env_name == "GITHUB_OUTPUT":
        sys.stdout.write(text)


def main(argv: Sequence[str] | None = None, gh: Gh = run_gh) -> int:
    """Command-line entry point.

    Args:
        argv: Arguments without the program name; ``sys.argv[1:]`` when None.
        gh: Runs the GitHub CLI (replaced in tests).

    Returns:
        0 for ``find`` and ``sarif`` whether or not anything is carried over
        (a full run is not an error); for ``verdict``, 0 once written and 1
        when the gate's environment is malformed.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("find", help="find the parent run an approval commit carries over")
    f.add_argument("--repo", required=True)
    f.add_argument("--workflow", required=True)
    f.add_argument("--head", required=True)
    f.add_argument("--head-repo", required=True)
    f.add_argument("--head-branch", required=True)
    f.add_argument("--kind", choices=("ci", "run"), required=True)
    f.add_argument("--dest", type=Path, default=Path(os.environ.get("RUNNER_TEMP", ".")) / "ci-carry")
    f.add_argument("--carry-out", type=Path, default=None, help="also write the parent run as JSON here")
    v = sub.add_parser("verdict", help="write this CI Gate run's verdict")
    v.add_argument("--out", type=Path, required=True)
    s = sub.add_parser("sarif", help="download a parent CodeQL run's SARIF for one language")
    s.add_argument("--repo", required=True)
    s.add_argument("--run-id", required=True)
    s.add_argument("--language", required=True)
    s.add_argument("--dest", type=Path, required=True)
    args = parser.parse_args(argv)

    if args.cmd == "verdict":
        try:
            doc = make_verdict(os.environ)
        except ExecError as exc:
            print(f"::error::ci_carry: {exc}", file=sys.stderr)
            return 1
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return 0

    if args.cmd == "sarif":
        args.dest.mkdir(parents=True, exist_ok=True)
        sarif, reason = kept_sarif(args.repo, args.run_id, args.language, args.dest, gh)
        if sarif is not None:
            _append("GITHUB_OUTPUT", f"sarif={sarif}\n")
        line = reason if sarif is not None else f"analyzed: {reason}"
        _append("GITHUB_STEP_SUMMARY", f"CodeQL {args.language}: {line}.\n")
        print(line, file=sys.stderr)
        return 0

    args.dest.mkdir(parents=True, exist_ok=True)
    run, parent, reason = find(
        repo=args.repo,
        workflow=args.workflow,
        head=args.head,
        head_repo=args.head_repo,
        head_branch=args.head_branch,
        kind=args.kind,
        dest=args.dest,
        gh=gh,
    )
    if run is not None:
        _append("GITHUB_OUTPUT", f"run-id={run.run_id}\nparent={parent}\n")
        if args.carry_out is not None:
            args.carry_out.parent.mkdir(parents=True, exist_ok=True)
            carried = {"run_id": run.run_id, "run_attempt": run.run_attempt, "head_sha": parent}
            args.carry_out.write_text(json.dumps(carried, sort_keys=True) + "\n", encoding="utf-8")
        line = f"Visual approval commit: {reason}."
    else:
        line = f"Full run: {reason}."
    _append("GITHUB_STEP_SUMMARY", line + "\n")
    print(line, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
