#!/usr/bin/env python3
"""Putting a pull request back in the merge queue after a runner never came.

``scripts/merge_queue_requeue.py`` may turn auto-merge back on only when a
merge-group CI run failed because GitHub never gave a job a runner, and only
a few times per pull request head. These tests drive its sans-IO checks with
GitHub API bodies (the seed corpus is trimmed from the run that removed #604
from the queue on 2026-11-03), replay and mutate the corpus against
independent oracles, and run its command line with a fake ``gh``.
"""

from __future__ import annotations

import copy
import importlib.util
import json
import os
import random
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "merge_queue_requeue_corpus"
REPO = "Bookclerk/bookclerk"
RUN_ID = 41660521252
STRANDED_JOB = 126002764377


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("merge_queue_requeue", ROOT / "scripts" / "merge_queue_requeue.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["merge_queue_requeue"] = module
    spec.loader.exec_module(module)
    return module


mq = _load()


def seed(name: str) -> bytes:
    return (CORPUS / name).read_bytes()


def edited(name: str, **over: object) -> bytes:
    doc = json.loads(seed(name))
    doc.update(over)
    return json.dumps(doc).encode()


class QueuedPrTests(unittest.TestCase):
    """Only failed merge-group runs of this repository's CI count."""

    def test_reads_the_pull_request_number(self) -> None:
        self.assertEqual(mq.queued_pr(seed("run__pr604.json"), REPO), 604)
        self.assertEqual(mq.queued_pr(edited("run__pr604.json", conclusion="cancelled"), REPO), 604)

    def test_rejects_other_runs(self) -> None:
        bad = {
            "pull request": seed("run__pull_request.json"),
            "success": seed("run__success.json"),
            "fork": seed("run__fork.json"),
            "other workflow": edited("run__pr604.json", path=".github/workflows/codeql.yml"),
            "running": edited("run__pr604.json", status="in_progress"),
            "other base branch": edited("run__pr604.json", head_branch="gh-readonly-queue/dev/pr-604-" + "a" * 40),
            "short sha": edited("run__pr604.json", head_branch="gh-readonly-queue/main/pr-604-abc"),
            "pr zero": edited("run__pr604.json", head_branch="gh-readonly-queue/main/pr-0-" + "a" * 40),
            "branch not string": edited("run__pr604.json", head_branch=604),
            "not json": b"{",
            "list": b"[]",
        }
        for name, raw in bad.items():
            with self.subTest(case=name):
                self.assertIsNone(mq.queued_pr(raw, REPO))
        self.assertIsNone(mq.queued_pr(seed("run__pr604.json"), "other/repo"))


class PrRunAttemptTests(unittest.TestCase):
    """Only failed pull request runs of this repository's CI count."""

    def test_reads_the_attempt(self) -> None:
        """Reads the attempt of a same-repository and a fork pull request run."""
        self.assertEqual(mq.pr_run_attempt(seed("run__pr605_attempt1.json"), REPO), 1)
        self.assertEqual(mq.pr_run_attempt(seed("run__pr_fork_attempt3.json"), REPO), 3)

    def test_rejects_other_runs(self) -> None:
        """Rejects runs that are not failed pull request CI runs of this repository."""
        bad = {
            "merge group": seed("run__pr604.json"),
            "cancelled": edited("run__pr605_attempt1.json", conclusion="cancelled"),
            "success": edited("run__pr605_attempt1.json", conclusion="success"),
            "running": edited("run__pr605_attempt1.json", status="in_progress"),
            "other workflow": edited("run__pr605_attempt1.json", path=".github/workflows/codeql.yml"),
            "other base repo": edited("run__pr605_attempt1.json", repository={"full_name": "x/y"}),
            "attempt zero": edited("run__pr605_attempt1.json", run_attempt=0),
            "attempt string": edited("run__pr605_attempt1.json", run_attempt="1"),
            "attempt bool": edited("run__pr605_attempt1.json", run_attempt=True),
            "not json": b"nope",
        }
        for name, raw in bad.items():
            with self.subTest(case=name):
                self.assertIsNone(mq.pr_run_attempt(raw, REPO))


class SupersededTests(unittest.TestCase):
    """A newer run on the same head branch and repository supersedes a run."""

    def test_lineage(self) -> None:
        """Reads head repository, branch and run number; rejects incomplete runs."""
        self.assertEqual(
            mq.run_lineage(seed("run__pr605_attempt1.json")),
            ("Bookclerk/bookclerk", "claude/project-thread-bslas8", 3541),
        )
        for name, raw in {
            "no number": edited("run__pr605_attempt1.json", run_number=None),
            "zero": edited("run__pr605_attempt1.json", run_number=0),
            "no branch": edited("run__pr605_attempt1.json", head_branch=None),
            "no head repo": edited("run__pr605_attempt1.json", head_repository=None),
        }.items():
            with self.subTest(case=name):
                self.assertIsNone(mq.run_lineage(raw))

    def test_newer_run(self) -> None:
        """Only a larger run number from the same head repository supersedes."""
        self.assertIs(mq.newer_run(seed("runs__pr605_current.json"), REPO, 3541), False)
        self.assertIs(mq.newer_run(seed("runs__pr605_superseded.json"), REPO, 3541), True)
        self.assertIs(mq.newer_run(seed("runs__fork_same_branch.json"), REPO, 3541), False)
        self.assertIs(mq.newer_run(seed("runs__fork_same_branch.json"), "someone/bookclerk", 3541), True)
        self.assertIsNone(mq.newer_run(b'{"workflow_runs": [1]}', REPO, 1))
        self.assertIsNone(mq.newer_run(b"[]", REPO, 1))


class StrandedJobsTests(unittest.TestCase):
    """Only jobs that never started, with nothing else wrong."""

    def test_the_604_run(self) -> None:
        self.assertEqual(mq.stranded_jobs(seed("jobs__pr604_stranded.json")), [STRANDED_JOB])

    def test_green_run_has_none(self) -> None:
        self.assertEqual(mq.stranded_jobs(seed("jobs__all_green.json")), [])

    def test_a_real_failure_stops_it(self) -> None:
        self.assertIsNone(mq.stranded_jobs(seed("jobs__real_failure.json")))
        self.assertIsNone(mq.stranded_jobs(seed("jobs__cancelled_after_start.json")))

    def test_followers_are_ignored_also_with_a_caller_prefix(self) -> None:
        jobs = [
            {"id": 1, "name": "ci / CI Gate", "conclusion": "failure"},
            {"id": 2, "name": "merge-queue fail-fast", "conclusion": "cancelled"},
            {"id": 3, "name": "tray (macos-latest)", "conclusion": "cancelled", "runner_name": None, "steps": None},
        ]
        self.assertEqual(mq.stranded_jobs(json.dumps({"jobs": jobs}).encode()), [3])

    def test_still_running_or_malformed(self) -> None:
        bad = [
            {"jobs": [{"id": 1, "name": "x", "conclusion": None}]},
            {"jobs": [{"id": "1", "name": "x", "conclusion": "success"}]},
            {"jobs": [{"id": True, "name": "x", "conclusion": "success"}]},
            {"jobs": [{"id": 1, "conclusion": "success"}]},
            {"jobs": ["x"]},
            {"jobs": {}},
            [],
        ]
        for doc in bad:
            with self.subTest(doc=doc):
                self.assertIsNone(mq.stranded_jobs(json.dumps(doc).encode()))
        self.assertIsNone(mq.stranded_jobs(b"\xff"))


class AnnotationTests(unittest.TestCase):
    """GitHub's failure annotation is what tells a dequeue from no runner."""

    def test_annotations(self) -> None:
        self.assertTrue(mq.not_acquired(seed("annotations__not_acquired.json")))
        self.assertFalse(mq.not_acquired(seed("annotations__notice_only.json")))
        self.assertFalse(mq.not_acquired(seed("annotations__empty.json")))
        self.assertFalse(mq.not_acquired(b"{}"))
        warning = [{"annotation_level": "warning", "message": "The job was not acquired by Runner of type hosted"}]
        self.assertFalse(mq.not_acquired(json.dumps(warning).encode()))


class PrHeadTests(unittest.TestCase):
    """Only an open, non-draft pull request from this repository without auto-merge."""

    def test_open(self) -> None:
        self.assertEqual(mq.pr_head(seed("pr__pr604_open.json"), REPO), "601c089641a459acd8cca2daa02905c3d33c1703")

    def test_rejects(self) -> None:
        bad = {
            "auto-merge set": seed("pr__auto_merge_set.json"),
            "draft": seed("pr__draft.json"),
            "closed": edited("pr__pr604_open.json", state="closed"),
            "draft missing": edited("pr__pr604_open.json", draft=None),
            "fork": edited("pr__pr604_open.json", head={"sha": "a" * 40, "repo": {"full_name": "x/y"}}),
            "deleted fork": edited("pr__pr604_open.json", head={"sha": "a" * 40, "repo": None}),
            "bad sha": edited("pr__pr604_open.json", head={"sha": "HEAD", "repo": {"full_name": REPO}}),
            "no head": edited("pr__pr604_open.json", head=None),
            "not json": b"",
        }
        for name, raw in bad.items():
            with self.subTest(case=name):
                self.assertIsNone(mq.pr_head(raw, REPO))


class TimelineTests(unittest.TestCase):
    """Enqueues are counted per head commit."""

    def test_counts(self) -> None:
        self.assertEqual(mq.enqueues_since_head(seed("timeline__pr604.json")), 1)
        self.assertEqual(mq.enqueues_since_head(seed("timeline__three_enqueues.json")), 3)
        self.assertEqual(mq.enqueues_since_head(seed("timeline__new_head.json")), 1)
        self.assertEqual(mq.enqueues_since_head(b'[[{"event": "head_ref_force_pushed"}]]'), 0)

    def test_malformed(self) -> None:
        for raw in (b"{}", b"[{}]", b'[["x"]]', b"nope"):
            with self.subTest(raw=raw):
                self.assertIsNone(mq.enqueues_since_head(raw))


class FakeGh:
    """Answers `gh api` calls from canned bodies keyed by the URL path."""

    def __init__(self, bodies: dict[str, bytes]) -> None:
        self.bodies = bodies
        self.calls: list[list[str]] = []

    def __call__(self, args: Sequence[str]) -> bytes:
        args = list(args)
        self.calls.append(args)
        assert args[0] == "api", args
        url = args[-1].split("?")[0]
        if url not in self.bodies:
            raise subprocess.CalledProcessError(1, "gh")
        return self.bodies[url]


def happy_bodies() -> dict[str, bytes]:
    base = f"repos/{REPO}"
    return {
        f"{base}/actions/runs/{RUN_ID}": seed("run__pr604.json"),
        f"{base}/actions/runs/{RUN_ID}/jobs": seed("jobs__pr604_stranded.json"),
        f"{base}/check-runs/{STRANDED_JOB}/annotations": seed("annotations__not_acquired.json"),
        f"{base}/pulls/604": seed("pr__pr604_open.json"),
        f"{base}/issues/604/timeline": seed("timeline__pr604.json"),
    }


class DecideTests(unittest.TestCase):
    """End to end over a fake `gh`."""

    def test_the_604_ejection_is_enqueued_again(self) -> None:
        gh = FakeGh(happy_bodies())
        target, reason = mq.decide(REPO, RUN_ID, gh)
        self.assertEqual(target, (604, "601c089641a459acd8cca2daa02905c3d33c1703"))
        self.assertIn("never gave", reason)
        self.assertIn(["api", "--paginate", "--slurp", f"repos/{REPO}/issues/604/timeline?per_page=100"], gh.calls)

    def test_stays_out(self) -> None:
        base = f"repos/{REPO}"
        cases = {
            "not a merge group": {f"{base}/actions/runs/{RUN_ID}": seed("run__pull_request.json")},
            "real failure": {f"{base}/actions/runs/{RUN_ID}/jobs": seed("jobs__real_failure.json")},
            "nothing stranded": {f"{base}/actions/runs/{RUN_ID}/jobs": seed("jobs__all_green.json")},
            "dequeued by hand": {f"{base}/check-runs/{STRANDED_JOB}/annotations": seed("annotations__notice_only.json")},
            "already queued": {f"{base}/pulls/604": seed("pr__auto_merge_set.json")},
            "cap reached": {f"{base}/issues/604/timeline": seed("timeline__three_enqueues.json")},
            "timeline unreadable": {f"{base}/issues/604/timeline": b"{}"},
            "pushed after the failure": {
                f"{base}/issues/604/timeline": json.dumps(
                    json.loads(seed("timeline__pr604.json"))
                    + [[{"event": "committed", "sha": "c" * 40}]]
                ).encode()
            },
        }
        for name, over in cases.items():
            with self.subTest(case=name):
                bodies = happy_bodies()
                bodies.update(over)
                target, reason = mq.decide(REPO, RUN_ID, FakeGh(bodies))
                self.assertIsNone(target)
                self.assertTrue(reason)
                if name == "pushed after the failure":
                    self.assertIn("new head", reason)

    def test_api_failure_stays_out(self) -> None:
        bodies = happy_bodies()
        del bodies[f"repos/{REPO}/pulls/604"]
        target, reason = mq.decide(REPO, RUN_ID, FakeGh(bodies))
        self.assertIsNone(target)
        self.assertIn("failed", reason)


PR_RUN_ID = 41717878100


def rerun_bodies() -> dict[str, bytes]:
    base = f"repos/{REPO}"
    jobs = json.loads(seed("jobs__pr605_two_stranded.json"))
    stranded = [j["id"] for j in jobs["jobs"] if j["conclusion"] == "cancelled"]
    bodies = {
        f"{base}/actions/runs/{PR_RUN_ID}": seed("run__pr605_attempt1.json"),
        f"{base}/actions/runs/{PR_RUN_ID}/jobs": seed("jobs__pr605_two_stranded.json"),
        f"{base}/actions/workflows/ci.yml/runs": seed("runs__pr605_current.json"),
    }
    for job_id in stranded:
        bodies[f"{base}/check-runs/{job_id}/annotations"] = seed("annotations__not_acquired.json")
    return bodies


class DecideRerunTests(unittest.TestCase):
    """Pull request runs: re-run only stranded jobs, at most twice."""

    def test_the_605_run_is_re_run(self) -> None:
        """The run #605 lost to missing macOS runners is re-run."""
        rerun, reason = mq.decide_rerun(REPO, PR_RUN_ID, FakeGh(rerun_bodies()))
        self.assertTrue(rerun)
        self.assertIn("never gave", reason)

    def test_stays_failed(self) -> None:
        """Each refusal reason keeps the run failed."""
        base = f"repos/{REPO}"
        jobs = json.loads(seed("jobs__pr605_two_stranded.json"))
        first = next(j["id"] for j in jobs["jobs"] if j["conclusion"] == "cancelled")
        cases = {
            "third attempt": {f"{base}/actions/runs/{PR_RUN_ID}": edited("run__pr605_attempt1.json", run_attempt=3)},
            "merge group": {f"{base}/actions/runs/{PR_RUN_ID}": seed("run__pr604.json")},
            "real failure": {f"{base}/actions/runs/{PR_RUN_ID}/jobs": seed("jobs__real_failure.json")},
            "nothing stranded": {f"{base}/actions/runs/{PR_RUN_ID}/jobs": seed("jobs__all_green.json")},
            "cancelled by a push": {f"{base}/check-runs/{first}/annotations": seed("annotations__empty.json")},
            "superseded before recovery ran": {
                f"{base}/actions/workflows/ci.yml/runs": seed("runs__pr605_superseded.json")
            },
            "run listing unreadable": {f"{base}/actions/workflows/ci.yml/runs": b"[]"},
            "no head branch": {
                f"{base}/actions/runs/{PR_RUN_ID}": edited("run__pr605_attempt1.json", head_branch="")
            },
        }
        for name, over in cases.items():
            with self.subTest(case=name):
                bodies = rerun_bodies()
                bodies.update(over)
                rerun, reason = mq.decide_rerun(REPO, PR_RUN_ID, FakeGh(bodies))
                self.assertFalse(rerun)
                self.assertTrue(reason)
                if name == "superseded before recovery ran":
                    self.assertIn("superseded", reason)

    def test_lists_runs_of_the_same_branch(self) -> None:
        """The last call lists pull request CI runs of the run's head branch."""
        gh = FakeGh(rerun_bodies())
        self.assertTrue(mq.decide_rerun(REPO, PR_RUN_ID, gh)[0])
        self.assertEqual(
            gh.calls[-1],
            [
                "api",
                f"repos/{REPO}/actions/workflows/ci.yml/runs"
                "?event=pull_request&branch=claude%2Fproject-thread-bslas8&per_page=100",
            ],
        )

    def test_a_fork_branch_of_the_same_name_does_not_supersede(self) -> None:
        """A newer run from a fork's same-named branch is another pull request's."""
        bodies = rerun_bodies()
        bodies[f"repos/{REPO}/actions/workflows/ci.yml/runs"] = seed("runs__fork_same_branch.json")
        self.assertTrue(mq.decide_rerun(REPO, PR_RUN_ID, FakeGh(bodies))[0])

    def test_a_fork_pull_request_run_is_re_run(self) -> None:
        """A fork's run on attempts 1 and 2 is re-run like any other."""
        for attempt in (1, 2):
            with self.subTest(attempt=attempt):
                bodies = rerun_bodies()
                bodies[f"repos/{REPO}/actions/runs/{PR_RUN_ID}"] = edited(
                    "run__pr605_attempt1.json",
                    run_attempt=attempt,
                    head_repository={"full_name": "someone/bookclerk"},
                )
                self.assertTrue(mq.decide_rerun(REPO, PR_RUN_ID, FakeGh(bodies))[0])

    def test_second_attempt_still_re_runs(self) -> None:
        """Attempt 2 is still re-run; attempt 3 is the cap."""
        bodies = rerun_bodies()
        bodies[f"repos/{REPO}/actions/runs/{PR_RUN_ID}"] = edited("run__pr605_attempt1.json", run_attempt=2)
        self.assertTrue(mq.decide_rerun(REPO, PR_RUN_ID, FakeGh(bodies))[0])

    def test_api_failure_stays_failed(self) -> None:
        """A failed API call keeps the run failed."""
        rerun, reason = mq.decide_rerun(REPO, PR_RUN_ID, FakeGh({}))
        self.assertFalse(rerun)
        self.assertIn("failed", reason)


class RunGhTests(unittest.TestCase):
    """A stalled `gh` call becomes a handled failure instead of holding the job."""

    def test_timeout_becomes_a_handled_failure(self) -> None:
        old = mq.subprocess.run

        def stall(*args: object, **kwargs: object) -> object:
            self.assertEqual(kwargs.get("timeout"), mq.GH_TIMEOUT_SECONDS)
            raise subprocess.TimeoutExpired(cmd=["gh"], timeout=1)

        mq.subprocess.run = stall  # type: ignore[assignment]
        try:
            with self.assertRaises(subprocess.CalledProcessError):
                mq.run_gh(["api", "x"])
        finally:
            mq.subprocess.run = old


MACOS_JOB = "macOS (sandbox, gateway, tray)"
MACOS_JOB_ID = 126203811708


def stranded_bodies(attempt: int = 2) -> dict[str, bytes]:
    """A pull request run whose CI Gate is deciding on attempt ``attempt``."""
    base = f"repos/{REPO}"
    return {
        f"{base}/actions/runs/{PR_RUN_ID}": seed("run__pr_gate_attempt2.json"),
        f"{base}/actions/runs/{PR_RUN_ID}/attempts/{attempt}/jobs": seed("jobs__macos_stranded.json"),
        f"{base}/check-runs/{MACOS_JOB_ID}/annotations": seed("annotations__not_acquired.json"),
    }


def merge_group_gate_bodies() -> dict[str, bytes]:
    """The 604 merge group, as its CI Gate sees it, with the macOS job stranded."""
    base = f"repos/{REPO}"
    return {
        f"{base}/actions/runs/{RUN_ID}": edited(
            "run__pr604.json", status="in_progress", conclusion=None, repository={"full_name": REPO}
        ),
        f"{base}/actions/runs/{RUN_ID}/attempts/1/jobs": seed("jobs__macos_stranded.json"),
        f"{base}/check-runs/{MACOS_JOB_ID}/annotations": seed("annotations__not_acquired.json"),
        f"{base}/issues/604/timeline": seed("timeline__pr604.json"),
    }


class StrandedJobTests(unittest.TestCase):
    """CI Gate passes a stranded macOS job only after one retry."""

    def test_reads_the_gate_run(self) -> None:
        raw = seed("run__pr_gate_attempt2.json")
        self.assertEqual(mq.gate_run(raw, REPO), ("pull_request", "claude/project-thread-bslas8"))
        queue = edited("run__pr604.json", repository={"full_name": REPO})
        self.assertEqual(mq.gate_run(queue, REPO), ("merge_group", "gh-readonly-queue/main/pr-604-" + "9bc9f97b61370d35f573f5f1dc23b99d75eb155b"))
        self.assertIsNone(mq.gate_run(seed("run__pr604.json"), REPO), "no base repository")
        for name, bad in {
            "push": edited("run__pr_gate_attempt2.json", event="push"),
            "other workflow": edited("run__pr_gate_attempt2.json", path=".github/workflows/osv-remediate.yml"),
            "no branch": edited("run__pr_gate_attempt2.json", head_branch=""),
            "not json": b"{",
        }.items():
            with self.subTest(case=name):
                self.assertIsNone(mq.gate_run(bad, REPO))
        self.assertIsNone(mq.gate_run(seed("run__pr_gate_attempt2.json"), "other/repo"))

    def test_finds_only_a_stranded_job_by_name(self) -> None:
        raw = seed("jobs__macos_stranded.json")
        self.assertEqual(mq.stranded_job(raw, MACOS_JOB), MACOS_JOB_ID)
        self.assertIsNone(mq.stranded_job(raw, "tray (windows-latest)"), "it succeeded")
        self.assertIsNone(mq.stranded_job(raw, "nope"))
        doc = json.loads(raw)
        prefixed = copy.deepcopy(doc)
        for job in prefixed["jobs"]:
            job["name"] = "verify / " + job["name"]
        self.assertEqual(mq.stranded_job(json.dumps(prefixed).encode(), MACOS_JOB), MACOS_JOB_ID)
        twice = copy.deepcopy(doc)
        twice["jobs"].append(next(j for j in doc["jobs"] if j["name"] == MACOS_JOB))
        self.assertIsNone(mq.stranded_job(json.dumps(twice).encode(), MACOS_JOB))
        started = copy.deepcopy(doc)
        next(j for j in started["jobs"] if j["name"] == MACOS_JOB)["runner_name"] = "GitHub Actions 1"
        self.assertIsNone(mq.stranded_job(json.dumps(started).encode(), MACOS_JOB))

    def test_gate_passed(self) -> None:
        self.assertFalse(mq.gate_passed(seed("jobs__macos_stranded.json")))
        self.assertFalse(mq.gate_passed(seed("jobs__pr605_two_stranded.json")))
        jobs = json.loads(seed("jobs__pr605_two_stranded.json"))
        next(j for j in jobs["jobs"] if j["name"] == "CI Gate")["conclusion"] = "success"
        self.assertTrue(mq.gate_passed(json.dumps(jobs).encode()))

    def test_pull_request_passes_on_the_second_attempt(self) -> None:
        passes, reason = mq.decide_stranded(REPO, PR_RUN_ID, 2, MACOS_JOB, FakeGh(stranded_bodies()))
        self.assertTrue(passes, reason)

    def test_pull_request_first_attempt_is_re_run_first(self) -> None:
        bodies = stranded_bodies(1)
        bodies[f"repos/{REPO}/actions/runs/{PR_RUN_ID}"] = edited("run__pr_gate_attempt2.json", run_attempt=1)
        passes, reason = mq.decide_stranded(REPO, PR_RUN_ID, 1, MACOS_JOB, FakeGh(bodies))
        self.assertFalse(passes)
        self.assertIn("re-run once first", reason)

    def test_merge_group_passes_after_one_re_enqueue(self) -> None:
        bodies = merge_group_gate_bodies()
        passes, reason = mq.decide_stranded(REPO, RUN_ID, 1, MACOS_JOB, FakeGh(bodies))
        self.assertFalse(passes, "the 604 timeline holds one enqueue")
        self.assertIn("re-enqueued once first", reason)
        timeline = json.loads(seed("timeline__pr604.json")) + [[{"event": "added_to_merge_queue"}]]
        bodies[f"repos/{REPO}/issues/604/timeline"] = json.dumps(timeline).encode()
        passes, reason = mq.decide_stranded(REPO, RUN_ID, 1, MACOS_JOB, FakeGh(bodies))
        self.assertTrue(passes, reason)

    def test_refusals(self) -> None:
        base = f"repos/{REPO}"
        cases = {
            "main push": {f"{base}/actions/runs/{PR_RUN_ID}": edited("run__pr_gate_attempt2.json", event="push")},
            "job ran": {f"{base}/actions/runs/{PR_RUN_ID}/attempts/2/jobs": seed("jobs__all_green.json")},
            "cancelled by hand": {f"{base}/check-runs/{MACOS_JOB_ID}/annotations": seed("annotations__notice_only.json")},
            "api failure": {f"{base}/check-runs/{MACOS_JOB_ID}/annotations": None},
        }
        for name, over in cases.items():
            with self.subTest(case=name):
                bodies = stranded_bodies()
                for key, body in over.items():
                    if body is None:
                        del bodies[key]
                    else:
                        bodies[key] = body
                passes, reason = mq.decide_stranded(REPO, PR_RUN_ID, 2, MACOS_JOB, FakeGh(bodies))
                self.assertFalse(passes)
                self.assertTrue(reason)
        passes, _ = mq.decide_stranded(REPO, PR_RUN_ID, 0, MACOS_JOB, FakeGh({}))
        self.assertFalse(passes)

    def test_recovery_leaves_a_run_whose_gate_passed(self) -> None:
        """A run CI Gate let through is never re-run or re-enqueued."""
        bodies = rerun_bodies()
        jobs = json.loads(seed("jobs__pr605_two_stranded.json"))
        next(j for j in jobs["jobs"] if j["name"] == "CI Gate")["conclusion"] = "success"
        bodies[f"repos/{REPO}/actions/runs/{PR_RUN_ID}/jobs"] = json.dumps(jobs).encode()
        rerun, reason = mq.decide_rerun(REPO, PR_RUN_ID, FakeGh(bodies))
        self.assertFalse(rerun)
        self.assertIn("passed CI Gate", reason)


class CliTests(unittest.TestCase):
    """The command line writes `pr=` only when the pull request goes back."""

    def run_main(self, args: list[str], gh: FakeGh) -> tuple[int, str, str]:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            summary = Path(tmp) / "summary"
            out.touch()
            summary.touch()
            old = {k: os.environ.get(k) for k in ("GITHUB_OUTPUT", "GITHUB_STEP_SUMMARY")}
            os.environ["GITHUB_OUTPUT"] = str(out)
            os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
            try:
                code = mq.main(args, gh=gh)
            finally:
                for k, v in old.items():
                    if v is None:
                        os.environ.pop(k, None)
                    else:
                        os.environ[k] = v
            return code, out.read_text(encoding="utf-8"), summary.read_text(encoding="utf-8")

    def test_requeue_writes_the_number(self) -> None:
        code, out, summary = self.run_main(["--repo", REPO, "--run-id", str(RUN_ID)], FakeGh(happy_bodies()))
        self.assertEqual(
            (code, out), (0, "pr=604\nsha=601c089641a459acd8cca2daa02905c3d33c1703\n")
        )
        self.assertIn("#604", summary)

    def test_stay_out_writes_nothing(self) -> None:
        bodies = happy_bodies()
        bodies[f"repos/{REPO}/actions/runs/{RUN_ID}/jobs"] = seed("jobs__real_failure.json")
        code, out, _ = self.run_main(["--repo", REPO, "--run-id", str(RUN_ID)], FakeGh(bodies))
        self.assertEqual((code, out), (0, ""))

    def test_rerun_writes_the_flag(self) -> None:
        """An eligible run writes ``rerun=true`` and a summary line."""
        code, out, summary = self.run_main(
            ["--repo", REPO, "--run-id", str(PR_RUN_ID), "--rerun"], FakeGh(rerun_bodies())
        )
        self.assertEqual((code, out), (0, "rerun=true\n"))
        self.assertIn("Pull request re-run", summary)

    def test_no_rerun_writes_nothing(self) -> None:
        """A refused re-run writes no output."""
        code, out, _ = self.run_main(["--repo", REPO, "--run-id", str(PR_RUN_ID), "--rerun"], FakeGh({}))
        self.assertEqual((code, out), (0, ""))

    def test_stranded_writes_the_flag(self) -> None:
        """A stranded macOS job on a retried run writes ``stranded=true``."""
        args = ["--repo", REPO, "--run-id", str(PR_RUN_ID), "--run-attempt", "2", "--stranded-job", MACOS_JOB]
        code, out, summary = self.run_main(args, FakeGh(stranded_bodies()))
        self.assertEqual((code, out), (0, "stranded=true\n"))
        self.assertIn("Stranded job", summary)
        args[5] = "1"
        code, out, _ = self.run_main(args, FakeGh(stranded_bodies(1)))
        self.assertEqual((code, out), (0, ""))

    def test_bad_arguments_never_call_github(self) -> None:
        gh = FakeGh({})
        for args in (
            ["--repo", "x", "--run-id", "1"],
            ["--repo", REPO, "--run-id", "0"],
            ["--repo", "a/b c", "--run-id", "1"],
            ["--repo", REPO, "--run-id", "1", "--stranded-job", MACOS_JOB],
            ["--repo", REPO, "--run-id", "1", "--stranded-job", "", "--run-attempt", "2"],
        ):
            with self.subTest(args=args):
                code, out, _ = self.run_main(args, gh)
                self.assertEqual((code, out), (2, ""))
        self.assertEqual(gh.calls, [])


# Independent oracles, written with explicit walks and exact type checks
# instead of the script's helpers, for the corpus replay and fuzzing below.
def _parse(raw: bytes) -> object:
    try:
        return json.loads(raw.decode("utf-8"))
    except Exception:
        return None


def oracle_queued_pr(raw: bytes) -> int | None:
    run = _parse(raw)
    if type(run) is not dict:
        return None
    repo = run.get("head_repository")
    if not (
        run.get("event") == "merge_group"
        and run.get("path") == ".github/workflows/ci.yml"
        and run.get("status") == "completed"
        and run.get("conclusion") in ("failure", "cancelled")
        and type(repo) is dict
        and repo.get("full_name") == REPO
        and type(run.get("head_branch")) is str
    ):
        return None
    prefix = "gh-readonly-queue/main/pr-"
    branch: str = run["head_branch"]
    if not branch.startswith(prefix):
        return None
    number, _, sha = branch[len(prefix) :].partition("-")
    hexdigits = "0123456789abcdef"
    if not (1 <= len(number) <= 10 and number.isascii() and number.isdigit() and number[0] != "0"):
        return None
    if len(sha) != 40 or any(c not in hexdigits for c in sha):
        return None
    return int(number)


def oracle_stranded(raw: bytes) -> list[int] | None:
    doc = _parse(raw)
    if type(doc) is not dict or type(doc.get("jobs")) is not list:
        return None
    out = []
    for job in doc["jobs"]:
        if type(job) is not dict or type(job.get("name")) is not str or type(job.get("id")) is not int:
            return None
        name = job["name"]
        follower = name in ("CI Gate", "merge-queue fail-fast") or name.endswith((" / CI Gate", " / merge-queue fail-fast"))
        if job.get("conclusion") in ("success", "skipped") or follower:
            continue
        if job.get("conclusion") == "cancelled" and job.get("runner_name") in (None, "") and job.get("steps") in (None, []):
            out.append(job["id"])
            continue
        return None
    return out


def oracle_not_acquired(raw: bytes) -> bool:
    doc = _parse(raw)
    if type(doc) is not list:
        return False
    for a in doc:
        if type(a) is dict and a.get("annotation_level") == "failure" and type(a.get("message")) is str:
            if "was not acquired by Runner" in a["message"]:
                return True
    return False


def oracle_pr_head(raw: bytes) -> str | None:
    pr = _parse(raw)
    if type(pr) is not dict or type(pr.get("head")) is not dict:
        return None
    head = pr["head"]
    sha = head.get("sha")
    repo = head.get("repo")
    if pr.get("state") != "open" or pr.get("draft") is not False or "auto_merge" in pr and pr["auto_merge"] is not None:
        return None
    if type(repo) is not dict or repo.get("full_name") != REPO or type(sha) is not str:
        return None
    if len(sha) != 40 or any(c not in "0123456789abcdef" for c in sha):
        return None
    return sha


def oracle_enqueues(raw: bytes) -> int | None:
    pages = _parse(raw)
    if type(pages) is not list:
        return None
    events = []
    for page in pages:
        if type(page) is not list:
            return None
        for event in page:
            if type(event) is not dict:
                return None
            events.append(event.get("event"))
    last_head = max((i for i, e in enumerate(events) if e in ("committed", "head_ref_force_pushed")), default=-1)
    return sum(1 for e in events[last_head + 1 :] if e == "added_to_merge_queue")


def oracle_pr_run_attempt(raw: bytes) -> int | None:
    """Independent reading of ``pr_run_attempt`` with exact type checks."""
    run = _parse(raw)
    if type(run) is not dict or type(run.get("repository")) is not dict:
        return None
    attempt = run.get("run_attempt")
    if not (
        run.get("event") == "pull_request"
        and run.get("path") == ".github/workflows/ci.yml"
        and run.get("status") == "completed"
        and run.get("conclusion") == "failure"
        and run["repository"].get("full_name") == REPO
        and type(attempt) is int
        and attempt >= 1
    ):
        return None
    return attempt


def oracle_run_lineage(raw: bytes) -> tuple[str, str, int] | None:
    """Independent reading of ``run_lineage`` with exact type checks."""
    run = _parse(raw)
    if type(run) is not dict or type(run.get("head_repository")) is not dict:
        return None
    name = run["head_repository"].get("full_name")
    branch = run.get("head_branch")
    number = run.get("run_number")
    if type(name) is str and name != "" and type(branch) is str and branch != "" and type(number) is int and number > 0:
        return name, branch, number
    return None


def oracle_newer_run(raw: bytes, head_repo: str, run_number: int) -> bool | None:
    """Independent reading of ``newer_run`` with exact type checks."""
    body = _parse(raw)
    if type(body) is not dict or type(body.get("workflow_runs")) is not list:
        return None
    runs = body["workflow_runs"]
    if not all(type(r) is dict and type(r.get("head_repository")) is dict and type(r.get("run_number")) is int for r in runs):
        return None
    return any(r["head_repository"].get("full_name") == head_repo and r["run_number"] > run_number for r in runs)


def oracle_gate_run(raw: bytes) -> tuple[str, str] | None:
    """Independent reading of ``gate_run`` with exact type checks."""
    run = _parse(raw)
    if type(run) is not dict or type(run.get("repository")) is not dict:
        return None
    event = run.get("event")
    branch = run.get("head_branch")
    if (
        event in ("pull_request", "merge_group")
        and run.get("path") == ".github/workflows/ci.yml"
        and run["repository"].get("full_name") == REPO
        and type(branch) is str
        and branch != ""
    ):
        return event, branch
    return None


def oracle_stranded_job(raw: bytes, name: str) -> int | None:
    """Independent reading of ``stranded_job`` with exact type checks."""
    doc = _parse(raw)
    if type(doc) is not dict or type(doc.get("jobs")) is not list:
        return None
    if not all(type(j) is dict and type(j.get("name")) is str for j in doc["jobs"]):
        return None
    found = [j for j in doc["jobs"] if j["name"] == name or j["name"].endswith(" / " + name)]
    if len(found) != 1:
        return None
    job = found[0]
    if (
        type(job.get("id")) is int
        and job.get("status") == "completed"
        and job.get("conclusion") == "cancelled"
        and job.get("runner_name") in (None, "")
        and job.get("steps") in (None, [])
    ):
        return job["id"]
    return None


def oracle_gate_passed(raw: bytes) -> bool:
    """Independent reading of ``gate_passed`` with exact type checks."""
    doc = _parse(raw)
    if type(doc) is not dict or type(doc.get("jobs")) is not list:
        return False
    for job in doc["jobs"]:
        if type(job) is dict and type(job.get("name")) is str and job.get("conclusion") == "success":
            if job["name"] == "CI Gate" or job["name"].endswith(" / CI Gate"):
                return True
    return False


def check_against_oracles(case: unittest.TestCase, raw: bytes, label: str) -> None:
    """Asserts every parser agrees with its oracle on one body."""
    case.assertEqual(mq.run_lineage(raw), oracle_run_lineage(raw), label)
    for number in (1, 3541, 3551):
        case.assertEqual(mq.newer_run(raw, REPO, number), oracle_newer_run(raw, REPO, number), label)
    case.assertEqual(mq.pr_run_attempt(raw, REPO), oracle_pr_run_attempt(raw), label)
    case.assertEqual(mq.queued_pr(raw, REPO), oracle_queued_pr(raw), label)
    case.assertEqual(mq.stranded_jobs(raw), oracle_stranded(raw), label)
    case.assertEqual(mq.not_acquired(raw), oracle_not_acquired(raw), label)
    case.assertEqual(mq.pr_head(raw, REPO), oracle_pr_head(raw), label)
    case.assertEqual(mq.enqueues_since_head(raw), oracle_enqueues(raw), label)
    case.assertEqual(mq.gate_run(raw, REPO), oracle_gate_run(raw), label)
    for name in ("macOS (sandbox, gateway, tray)", "CI Gate", "tray (macos-latest)"):
        case.assertEqual(mq.stranded_job(raw, name), oracle_stranded_job(raw, name), label)
    case.assertEqual(mq.gate_passed(raw), oracle_gate_passed(raw), label)


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus (``run__``, ``runs__``, ``jobs__``, ``annotations__``, ``pr__``, ``timeline__``)."""

    def test_corpus_matches_oracles(self) -> None:
        """Every seed parses the same through the parsers and the oracles."""
        seeds = sorted(CORPUS.iterdir())
        self.assertGreaterEqual(len(seeds), 15)
        for path in seeds:
            with self.subTest(seed=path.name):
                self.assertIn(path.name.split("__")[0], ("run", "runs", "jobs", "annotations", "pr", "timeline"))
                check_against_oracles(self, path.read_bytes(), path.name)


class FuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing: never raises, always agrees with the oracles."""

    VALUES: list[object] = [
        None, True, False, 0, 1, -1, 1.5, "", "x", "success", "cancelled", "failure", "skipped",
        "merge_group", "pull_request", "open", "CI Gate", "macOS (sandbox, gateway, tray)", "completed", "added_to_merge_queue",
        "committed", REPO, [], {}, [1], {"a": 1},
    ]  # fmt: skip

    def mutate_json(self, rng: random.Random, node: object) -> object:
        if isinstance(node, dict) and node and rng.random() < 0.75:
            key = rng.choice(list(node))
            out = dict(node)
            if rng.random() < 0.2:
                del out[key]
            else:
                out[key] = self.mutate_json(rng, node[key])
            return out
        if isinstance(node, list) and node and rng.random() < 0.75:
            out = list(node)
            i = rng.randrange(len(out))
            out[i] = self.mutate_json(rng, out[i])
            return out
        if isinstance(node, str) and node and rng.random() < 0.5:
            i = rng.randrange(len(node))
            return node[:i] + rng.choice("0aZ-/ é") + node[i + 1 :]
        return copy.deepcopy(rng.choice(self.VALUES))

    def mutate_bytes(self, rng: random.Random, raw: bytes) -> bytes:
        data = bytearray(raw)
        for _ in range(rng.randint(1, 4)):
            pos = rng.randint(0, len(data))
            op = rng.random()
            if op < 0.4:
                data[pos:pos] = bytes([rng.choice(b'{}[]",:0aA \\\xff')])
            elif op < 0.7 and data:
                del data[min(pos, len(data) - 1)]
            elif data:
                data[min(pos, len(data) - 1)] = rng.randrange(256)
        return bytes(data)

    def test_structural_mutations(self) -> None:
        rng = random.Random(20261103)
        seeds = [json.loads(p.read_bytes()) for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            node = rng.choice(seeds)
            for _ in range(rng.randint(1, 3)):
                node = self.mutate_json(rng, node)
            raw = json.dumps(node).encode()
            check_against_oracles(self, raw, f"iteration {i}: {raw[:300]!r}")

    def test_byte_mutations(self) -> None:
        rng = random.Random(604)
        seeds = [p.read_bytes() for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            raw = self.mutate_bytes(rng, rng.choice(seeds))
            check_against_oracles(self, raw, f"iteration {i}: {raw[:300]!r}")


if __name__ == "__main__":
    unittest.main()
