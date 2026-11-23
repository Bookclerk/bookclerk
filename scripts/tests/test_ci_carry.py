#!/usr/bin/env python3
"""Carrying results over a visual approval commit: selection, replay and CLI.

``scripts/ci_carry.py`` may carry a pull request's results over only when
the new head changes nothing but ``ui/visual/baselines.lock`` since a parent
whose run passed. These tests drive its sans-IO selection with GitHub API
bodies, its gate replay with real plans from ``ci_plan.execute.resolve``,
and its command line with a fake ``gh``.
"""

from __future__ import annotations

import contextlib
import copy
import importlib.util
import io
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
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

from ci_plan.execute import ExecError, expected_jobs, resolve, validate  # noqa: E402

FIXTURES = Path(__file__).resolve().parent / "ci_plan_fixtures"
CORPUS = Path(__file__).resolve().parent / "ci_carry_corpus"
META = json.loads((FIXTURES / "workspace_metadata.json").read_text(encoding="utf-8"))

HEAD = "a" * 40
PARENT = "b" * 40
OTHER = "c" * 40
REPO = "Owner/repo"
BRANCH = "feature/x"
LOCK = "ui/visual/baselines.lock"
RUN_ID = 501
ATTEMPT = 1


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("ci_carry", SCRIPTS / "ci_carry.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["ci_carry"] = module
    spec.loader.exec_module(module)
    return module


cc = _load()


def commit_body(parents: Sequence[str] = (PARENT,), files: Sequence[dict] | None = None) -> bytes:
    """A commit API body for an approval commit unless overridden."""
    if files is None:
        files = [{"filename": LOCK, "status": "modified"}]
    return json.dumps({"sha": HEAD, "parents": [{"sha": p} for p in parents], "files": list(files)}).encode()


def run(run_id: int = RUN_ID, **over: object) -> dict:
    """A workflow run that qualifies unless a field is overridden."""
    doc: dict = {
        "id": run_id,
        "run_attempt": ATTEMPT,
        "head_sha": PARENT,
        "head_branch": BRANCH,
        "head_repository": {"full_name": REPO},
        "event": "pull_request",
        "status": "completed",
        "conclusion": "failure",
        "created_at": f"2026-11-23T04:00:{run_id % 60:02d}Z",
    }
    doc.update(over)
    return doc


def runs_body(*runs: dict) -> bytes:
    return json.dumps({"total_count": len(runs), "workflow_runs": list(runs)}).encode()


def parent_plan(**over: object) -> dict:
    """A ready pull request plan for the parent commit that runs the visual job."""
    art = resolve(
        base=OTHER,
        head=PARENT,
        selective_ci="1",
        run_id=str(RUN_ID),
        checkout_sha="d" * 40,
        event="pull_request",
        linux_only=True,
        paths=["ui/src/App.tsx", "crates/bookclerk-library/src/lib.rs"],
        metadata=META,
        workspace=ROOT,
    )
    art.update(over)
    return art


def verdict_for(plan: dict, visual: str = "failure", **over: object) -> dict:
    """The gate verdict of a run where everything but ``visual`` passed."""
    needs = {"plan": {"result": "success"}}
    for job, on in expected_jobs(plan).items():
        needs[job] = {"result": "success" if on else "skipped"}
    needs["ui-visual"] = {"result": visual}
    doc: dict = {
        "run_id": RUN_ID,
        "run_attempt": ATTEMPT,
        "needs": needs,
        "stranded": [],
        "coverage": "success",
        "contract": "success",
    }
    doc.update(over)
    return doc


class FakeGh:
    """Answers the `gh` calls `find` and `kept_sarif` make."""

    def __init__(
        self,
        *,
        commit: bytes | None = None,
        runs: bytes | None = None,
        artifacts: dict[str, bytes] | None = None,
        fail: Sequence[str] = (),
    ) -> None:
        self.commit = commit_body() if commit is None else commit
        self.runs = runs_body(run()) if runs is None else runs
        self.artifacts = artifacts or {}
        self.fail = set(fail)
        self.calls: list[list[str]] = []

    def __call__(self, args: Sequence[str]) -> bytes:
        args = list(args)
        self.calls.append(args)
        if args[0] == "api" and args[1].endswith(f"/commits/{HEAD}"):
            if "commit" in self.fail:
                raise subprocess.CalledProcessError(1, args)
            return self.commit
        if args[0] == "api" and "/runs" in args[3]:
            if "runs" in self.fail:
                raise subprocess.CalledProcessError(1, args)
            return self.runs
        if args[0] == "run" and args[1] == "download":
            name = args[args.index("-n") + 1]
            dest = Path(args[args.index("-D") + 1])
            if name not in self.artifacts:
                raise subprocess.CalledProcessError(1, args)
            dest.mkdir(parents=True, exist_ok=True)
            suffix = ".sarif" if name.startswith("codeql-sarif-") else ".json"
            (dest / f"file{suffix}").write_bytes(self.artifacts[name])
            return b""
        raise AssertionError(f"unexpected gh call {args}")


def ci_artifacts(plan: dict | None = None, verdict: dict | None = None) -> dict[str, bytes]:
    plan = parent_plan() if plan is None else plan
    verdict = verdict_for(plan) if verdict is None else verdict
    return {
        "ci-plan": json.dumps(plan).encode(),
        f"ci-gate-verdict-{ATTEMPT}": json.dumps(verdict).encode(),
    }


def find(gh: FakeGh, kind: str = "ci", **over: str):
    args = {
        "repo": REPO,
        "workflow": "ci.yml",
        "head": HEAD,
        "head_repo": REPO,
        "head_branch": BRANCH,
    }
    args.update(over)
    with tempfile.TemporaryDirectory() as tmp:
        return cc.find(kind=kind, dest=Path(tmp), gh=gh, **args)


class LockOnlyParentTests(unittest.TestCase):
    def test_an_approval_commit_names_its_parent(self) -> None:
        self.assertEqual(cc.lock_only_parent(commit_body()), PARENT)

    def test_any_other_commit_is_refused(self) -> None:
        cases = {
            "merge": commit_body(parents=(PARENT, OTHER)),
            "root": commit_body(parents=()),
            "two files": commit_body(
                files=[{"filename": LOCK, "status": "modified"}, {"filename": "ui/src/App.tsx", "status": "modified"}]
            ),
            "other file": commit_body(files=[{"filename": "ui/visual/baselines.lock.bak", "status": "modified"}]),
            "added": commit_body(files=[{"filename": LOCK, "status": "added"}]),
            "removed": commit_body(files=[{"filename": LOCK, "status": "removed"}]),
            "renamed": commit_body(
                files=[{"filename": LOCK, "status": "modified", "previous_filename": "ui/src/x.ts"}]
            ),
            "no files": commit_body(files=[]),
            "short parent": commit_body(parents=("b" * 12,)),
            "not json": b"\xff{",
            "list": b"[]",
        }
        for name, body in cases.items():
            with self.subTest(name):
                self.assertIsNone(cc.lock_only_parent(body))


class NewestRunTests(unittest.TestCase):
    def test_the_newest_matching_run_wins(self) -> None:
        body = runs_body(run(1, created_at="2026-11-23T01:00:00Z"), run(2, created_at="2026-11-23T02:00:00Z"))
        picked = cc.newest_run(body, PARENT, REPO, BRANCH)
        self.assertIsNotNone(picked)
        self.assertEqual(picked.run_id, 2)

    def test_runs_for_another_commit_branch_repo_or_event_are_ignored(self) -> None:
        for over in (
            {"head_sha": OTHER},
            {"head_branch": "main"},
            {"head_repository": {"full_name": "fork/repo"}},
            {"head_repository": None},
            {"event": "push"},
            {"id": True},
            {"run_attempt": 0},
        ):
            with self.subTest(over):
                self.assertIsNone(cc.newest_run(runs_body(run(**over)), PARENT, REPO, BRANCH))

    def test_an_unfinished_newer_run_is_still_the_newest(self) -> None:
        body = runs_body(
            run(1, created_at="2026-11-23T01:00:00Z", conclusion="success"),
            run(2, created_at="2026-11-23T02:00:00Z", status="in_progress", conclusion=None),
        )
        picked = cc.newest_run(body, PARENT, REPO, BRANCH)
        self.assertEqual((picked.run_id, picked.status, picked.conclusion), (2, "in_progress", ""))

    def test_malformed_bodies_pick_nothing(self) -> None:
        for body in (b"", b"{}", b'{"workflow_runs": 3}', b'{"workflow_runs": [1, "x"]}'):
            self.assertIsNone(cc.newest_run(body, PARENT, REPO, BRANCH))


class VerdictReplayTests(unittest.TestCase):
    def setUp(self) -> None:
        self.plan = parent_plan()
        self.run = cc.ParentRun(RUN_ID, ATTEMPT, "completed", "failure")

    def problem(self, plan: object = None, verdict: object = None) -> str | None:
        plan = self.plan if plan is None else plan
        verdict = verdict_for(self.plan) if verdict is None else verdict
        return cc.ci_verdict_problem(plan, verdict, self.run, PARENT)

    def test_a_run_red_only_on_visuals_is_carried(self) -> None:
        self.assertTrue(expected_jobs(self.plan)["ui-visual"])
        self.assertIsNone(self.problem())
        self.assertIsNone(self.problem(verdict=verdict_for(self.plan, visual="success")))

    def test_another_failed_job_is_not_carried(self) -> None:
        verdict = verdict_for(self.plan)
        verdict["needs"]["test"] = {"result": "failure"}
        self.assertIn("test: expected to run, result failure", self.problem(verdict=verdict))

    def test_a_job_that_ran_unplanned_or_went_missing_is_not_carried(self) -> None:
        skipped = next(job for job, on in expected_jobs(self.plan).items() if not on)
        verdict = verdict_for(self.plan)
        verdict["needs"][skipped] = {"result": "success"}
        self.assertIn(f"{skipped}: expected to skip", self.problem(verdict=verdict))
        verdict = verdict_for(self.plan)
        del verdict["needs"]["check"]
        self.assertIn("check: not reported", self.problem(verdict=verdict))
        verdict = verdict_for(self.plan)
        verdict["needs"]["plan"] = {"result": "failure"}
        self.assertIn("plan: failure", self.problem(verdict=verdict))

    def test_failed_or_missing_gate_steps_are_not_carried(self) -> None:
        for step in ("coverage", "contract"):
            for outcome in ("failure", "skipped", "cancelled", None, "weird"):
                with self.subTest(step=step, outcome=outcome):
                    verdict = verdict_for(self.plan, **{step: outcome})
                    self.assertIsNotNone(self.problem(verdict=verdict))

    def test_a_verdict_from_another_attempt_or_run_is_not_carried(self) -> None:
        self.assertIn("another run", self.problem(verdict=verdict_for(self.plan, run_attempt=2)))
        self.assertIn("another run", self.problem(verdict=verdict_for(self.plan, run_id=RUN_ID + 1)))

    def test_a_draft_or_foreign_plan_is_not_carried(self) -> None:
        draft = resolve(
            base=OTHER,
            head=PARENT,
            selective_ci="1",
            run_id=str(RUN_ID),
            checkout_sha="d" * 40,
            event="pull_request",
            draft=True,
            linux_only=True,
            paths=["ui/src/App.tsx"],
            metadata=META,
            workspace=ROOT,
        )
        self.assertIn("draft", self.problem(plan=draft, verdict=verdict_for(draft)))
        self.assertIn("parent commit", self.problem(plan=parent_plan(head_sha=OTHER)))
        self.assertIn("parent commit", self.problem(plan=parent_plan(event="merge_group")))
        self.assertIn("not valid", self.problem(plan=parent_plan(run_id="999")))
        self.assertIn("not valid", self.problem(plan=parent_plan(schema_version=1)))
        self.assertIn("not a JSON object", self.problem(plan=[]))

    def test_a_malformed_verdict_is_not_carried(self) -> None:
        for verdict in ([], {"run_id": RUN_ID}, verdict_for(self.plan, needs=[]), verdict_for(self.plan, stranded="x")):
            with self.subTest(verdict=verdict):
                self.assertIsNotNone(self.problem(verdict=verdict))
        bad = verdict_for(self.plan)
        bad["needs"]["test"] = "success"
        self.assertIsNotNone(self.problem(verdict=bad))

    def test_only_an_os_leg_may_be_stranded(self) -> None:
        verdict = verdict_for(self.plan, stranded=["test"])
        self.assertIn("only an OS leg job", self.problem(verdict=verdict))


class FindTests(unittest.TestCase):
    def test_ci_kind_carries_a_verified_parent(self) -> None:
        gh = FakeGh(artifacts=ci_artifacts())
        parent_run, parent, reason = find(gh)
        self.assertIsNotNone(parent_run, reason)
        self.assertEqual((parent_run.run_id, parent), (RUN_ID, PARENT))
        runs_call = next(c for c in gh.calls if "/runs" in "".join(c))
        self.assertIn(f"head_sha={PARENT}", runs_call)
        self.assertIn("event=pull_request", runs_call)

    def test_ci_kind_needs_both_artifacts(self) -> None:
        for missing in ("ci-plan", f"ci-gate-verdict-{ATTEMPT}"):
            artifacts = ci_artifacts()
            del artifacts[missing]
            parent_run, _, reason = find(FakeGh(artifacts=artifacts))
            self.assertIsNone(parent_run)
            self.assertIn("kept no plan or gate verdict", reason)

    def test_ci_kind_reads_the_latest_attempts_verdict(self) -> None:
        plan = parent_plan()
        gh = FakeGh(runs=runs_body(run(run_attempt=2)), artifacts=ci_artifacts(plan))
        parent_run, _, reason = find(gh)
        self.assertIsNone(parent_run)
        self.assertIn("kept no plan or gate verdict", reason)

    def test_ci_kind_refuses_a_failed_parent(self) -> None:
        plan = parent_plan()
        verdict = verdict_for(plan)
        verdict["needs"]["check"] = {"result": "failure"}
        parent_run, _, reason = find(FakeGh(artifacts=ci_artifacts(plan, verdict)))
        self.assertIsNone(parent_run)
        self.assertIn("can't be carried over", reason)

    def test_run_kind_needs_a_successful_finished_run(self) -> None:
        ok, _, _ = find(FakeGh(runs=runs_body(run(conclusion="success"))), kind="run", workflow="codeql.yml")
        self.assertIsNotNone(ok)
        for over in ({"conclusion": "failure"}, {"conclusion": "cancelled"}, {"status": "in_progress"}):
            with self.subTest(over):
                parent_run, _, _ = find(FakeGh(runs=runs_body(run(**over))), kind="run", workflow="codeql.yml")
                self.assertIsNone(parent_run)

    def test_anything_but_an_approval_commit_runs_in_full(self) -> None:
        gh = FakeGh(commit=commit_body(parents=(PARENT, OTHER)), artifacts=ci_artifacts())
        parent_run, _, reason = find(gh)
        self.assertIsNone(parent_run)
        self.assertIn("not an approval commit", reason)
        self.assertEqual(len(gh.calls), 1)

    def test_api_failures_and_missing_runs_run_in_full(self) -> None:
        for gh in (
            FakeGh(fail=["commit"]),
            FakeGh(fail=["runs"]),
            FakeGh(runs=runs_body()),
            FakeGh(runs=runs_body(run(head_repository={"full_name": "fork/repo"}))),
        ):
            parent_run, parent, _ = find(gh)
            self.assertIsNone(parent_run)
            self.assertEqual(parent, "")

    def test_unexpected_arguments_are_not_looked_up(self) -> None:
        for over in (
            {"head": "HEAD"},
            {"repo": "a/b/c"},
            {"head_branch": "x y"},
            {"workflow": "../ci.yml"},
            {"head_repo": ""},
        ):
            with self.subTest(over):
                gh = FakeGh()
                parent_run, _, _ = find(gh, **over)
                self.assertIsNone(parent_run)
                self.assertEqual(gh.calls, [])
        gh = FakeGh()
        self.assertIsNone(find(gh, kind="all")[0])
        self.assertEqual(gh.calls, [])


class SarifTests(unittest.TestCase):
    def sarif(self, category: str) -> bytes:
        return json.dumps(
            {
                "version": "2.1.0",
                "runs": [{"tool": {"driver": {"name": "CodeQL"}}, "automationDetails": {"id": category}}],
            }
        ).encode()

    def kept(self, gh: FakeGh, language: str = "python", run_id: str = str(RUN_ID)):
        with tempfile.TemporaryDirectory() as tmp:
            path, reason = cc.kept_sarif(REPO, run_id, language, Path(tmp), gh)
            return (path is not None), reason

    def test_a_matching_analysis_is_reused(self) -> None:
        gh = FakeGh(artifacts={"codeql-sarif-python": self.sarif("/language:python/")})
        ok, reason = self.kept(gh)
        self.assertTrue(ok, reason)

    def test_another_languages_analysis_is_refused(self) -> None:
        gh = FakeGh(artifacts={"codeql-sarif-python": self.sarif("/language:rust/")})
        ok, reason = self.kept(gh)
        self.assertFalse(ok)
        self.assertIn("/language:python", reason)

    def test_missing_artifacts_and_bad_input_analyze(self) -> None:
        self.assertFalse(self.kept(FakeGh())[0])
        self.assertFalse(self.kept(FakeGh(), language="cobol")[0])
        self.assertFalse(self.kept(FakeGh(), run_id="1;2")[0])


class VerdictTests(unittest.TestCase):
    def env(self, **over: str) -> dict[str, str]:
        doc = {
            "GITHUB_RUN_ID": str(RUN_ID),
            "GITHUB_RUN_ATTEMPT": "1",
            "NEEDS_JSON": json.dumps({"plan": {"result": "success", "outputs": {"x": "y"}}, "test": {"result": "failure"}}),
            "CI_GATE_STRANDED": "macos,",
            "COVERAGE_OUTCOME": "success",
            "CONTRACT_OUTCOME": "",
        }
        doc.update(over)
        return doc

    def test_the_verdict_keeps_results_only(self) -> None:
        doc = cc.make_verdict(self.env())
        self.assertEqual(doc["needs"], {"plan": {"result": "success"}, "test": {"result": "failure"}})
        self.assertEqual(doc["stranded"], ["macos"])
        self.assertEqual((doc["coverage"], doc["contract"]), ("success", "failure"))
        self.assertEqual((doc["run_id"], doc["run_attempt"]), (RUN_ID, 1))

    def test_a_malformed_environment_is_an_error(self) -> None:
        for over in ({"GITHUB_RUN_ID": ""}, {"GITHUB_RUN_ATTEMPT": "x"}, {"NEEDS_JSON": "[]"}, {"GITHUB_RUN_ID": "0"}):
            with self.subTest(over), self.assertRaises(ExecError):
                cc.make_verdict(self.env(**over))

    def test_a_written_verdict_round_trips_through_the_replay(self) -> None:
        plan = parent_plan()
        needs = verdict_for(plan)["needs"]
        env = self.env(NEEDS_JSON=json.dumps(needs), CI_GATE_STRANDED="", CONTRACT_OUTCOME="success")
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, env, clear=False):
            out = Path(tmp) / "v.json"
            self.assertEqual(cc.main(["verdict", "--out", str(out)]), 0)
            verdict = json.loads(out.read_text(encoding="utf-8"))
        problem = cc.ci_verdict_problem(plan, verdict, cc.ParentRun(RUN_ID, 1, "completed", "failure"), PARENT)
        self.assertIsNone(problem)


class ResolveTests(unittest.TestCase):
    CARRIED = {"run_id": RUN_ID, "run_attempt": ATTEMPT, "head_sha": PARENT}

    def carried(self, **over: object) -> dict:
        args: dict = {
            "base": PARENT,
            "head": HEAD,
            "selective_ci": "1",
            "run_id": "777",
            "checkout_sha": "e" * 40,
            "event": "pull_request",
            "linux_only": True,
            "paths": [LOCK],
            "metadata": META,
            "workspace": ROOT,
            "carried_from": self.CARRIED,
        }
        args.update(over)
        return resolve(**args)

    def test_a_carried_plan_runs_only_what_the_lock_selects(self) -> None:
        art = self.carried()
        self.assertEqual(art["carried_from"], self.CARRIED)
        validate(art, run_id="777", checkout_sha="e" * 40)
        jobs = {job for job, on in expected_jobs(art).items() if on}
        self.assertIn("ui-visual", jobs)
        self.assertNotIn("test", jobs)
        self.assertNotIn("e2e", jobs)

    def test_carrying_needs_a_ready_run_based_on_the_parent(self) -> None:
        with self.assertRaises(ExecError):
            self.carried(draft=True)
        with self.assertRaises(ExecError):
            self.carried(base=OTHER)
        with self.assertRaises(ExecError):
            self.carried(carried_from={"run_id": RUN_ID, "head_sha": PARENT})
        with self.assertRaises(ExecError):
            self.carried(carried_from={**self.CARRIED, "run_attempt": 0})

    def test_a_shadow_run_carries_nothing(self) -> None:
        self.assertIsNone(self.carried(selective_ci="0")["carried_from"])

    def test_validate_rejects_a_bad_carried_record(self) -> None:
        art = self.carried()
        for bad in ({"run_id": 1}, {**self.CARRIED, "head_sha": "x"}, {**self.CARRIED, "run_id": True}):
            broken = copy.deepcopy(art)
            broken["carried_from"] = bad
            with self.subTest(bad), self.assertRaises(ExecError):
                validate(broken, run_id=None, checkout_sha=None)
        broken = copy.deepcopy(art)
        broken["mode"] = "full"
        with self.assertRaises(ExecError):
            validate(broken, run_id=None, checkout_sha=None)
        old = copy.deepcopy(art)
        del old["carried_from"]
        validate(old, run_id=None, checkout_sha=None)


class CliTests(unittest.TestCase):
    def test_find_writes_outputs_and_the_carried_record(self) -> None:
        gh = FakeGh(artifacts=ci_artifacts())
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            summary = Path(tmp) / "summary"
            record = Path(tmp) / "carry" / "carry.json"
            env = {"GITHUB_OUTPUT": str(out), "GITHUB_STEP_SUMMARY": str(summary)}
            argv = [
                "find", "--repo", REPO, "--workflow", "ci.yml", "--head", HEAD, "--head-repo", REPO,
                "--head-branch", BRANCH, "--kind", "ci", "--dest", str(Path(tmp) / "dl"), "--carry-out", str(record),
            ]  # fmt: skip
            with mock.patch.dict(os.environ, env):
                self.assertEqual(cc.main(argv, gh=gh), 0)
            self.assertEqual(out.read_text(encoding="utf-8"), f"run-id={RUN_ID}\nparent={PARENT}\n")
            self.assertEqual(
                json.loads(record.read_text(encoding="utf-8")),
                {"run_id": RUN_ID, "run_attempt": ATTEMPT, "head_sha": PARENT},
            )
            self.assertIn("Visual approval commit", summary.read_text(encoding="utf-8"))

    def test_find_writes_no_output_for_a_full_run(self) -> None:
        gh = FakeGh(commit=commit_body(files=[]))
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            record = Path(tmp) / "carry.json"
            argv = [
                "find", "--repo", REPO, "--workflow", "ci.yml", "--head", HEAD, "--head-repo", REPO,
                "--head-branch", BRANCH, "--kind", "ci", "--dest", str(Path(tmp) / "dl"), "--carry-out", str(record),
            ]  # fmt: skip
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(out), "GITHUB_STEP_SUMMARY": ""}):
                self.assertEqual(cc.main(argv, gh=gh), 0)
            self.assertFalse(out.exists())
            self.assertFalse(record.exists())

    def test_resolve_reads_the_carried_record(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            record = Path(tmp) / "carry.json"
            plan = Path(tmp) / "plan" / "ci-plan.json"
            record.write_text(json.dumps({"run_id": RUN_ID, "run_attempt": ATTEMPT, "head_sha": PARENT}), encoding="utf-8")
            spec = importlib.util.spec_from_file_location("ci_exec", SCRIPTS / "ci-exec.py")
            assert spec and spec.loader
            ci_exec = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(ci_exec)
            env = {"GITHUB_OUTPUT": "", "GITHUB_STEP_SUMMARY": ""}
            quiet = contextlib.redirect_stdout(io.StringIO())
            with mock.patch.dict(os.environ, env), mock.patch.object(ci_exec, "git_head", return_value="e" * 40), quiet:
                argv = [
                    "--plan", str(plan), "resolve", "--base", PARENT, "--head", HEAD, "--paths", LOCK,
                    "--selective", "1", "--event", "pull_request", "--linux-only", "--carried-from", str(record),
                ]  # fmt: skip
                self.assertEqual(ci_exec.main(argv), 0)
                self.assertEqual(json.loads(plan.read_text(encoding="utf-8"))["carried_from"]["run_id"], RUN_ID)
                record.write_text("[]", encoding="utf-8")
                self.assertEqual(ci_exec.main(argv), 1)


# Independent oracles, written with explicit walks and exact type checks
# instead of the script's helpers, for the corpus replay and the mutation
# fuzzing below.
OS_LEG_JOBS = ("macos", "windows-cluster", "test-os")


def oracle_parent(raw: bytes) -> str | None:
    try:
        doc = json.loads(raw.decode("utf-8"))
    except Exception:
        return None
    if type(doc) is not dict or type(doc.get("parents")) is not list or type(doc.get("files")) is not list:
        return None
    if len(doc["parents"]) != 1 or len(doc["files"]) != 1:
        return None
    parent, change = doc["parents"][0], doc["files"][0]
    if type(parent) is not dict or type(parent.get("sha")) is not str or type(change) is not dict:
        return None
    sha = parent["sha"]
    if len(sha) != 40 or any(c not in "0123456789abcdef" for c in sha):
        return None
    if change.get("filename") != LOCK or change.get("status") != "modified" or "previous_filename" in change:
        return None
    return sha


def oracle_newest(raw: bytes) -> tuple[int, int, str, str] | None:
    try:
        doc = json.loads(raw.decode("utf-8"))
    except Exception:
        return None
    if type(doc) is not dict or type(doc.get("workflow_runs")) is not list:
        return None
    best = None
    for r in doc["workflow_runs"]:
        if type(r) is not dict:
            continue
        rid, attempt, repo = r.get("id"), r.get("run_attempt"), r.get("head_repository")
        if type(rid) is not int or rid <= 0 or type(attempt) is not int or attempt <= 0:
            continue
        if type(repo) is not dict or repo.get("full_name") != REPO or type(r.get("status")) is not str:
            continue
        if r.get("head_sha") != PARENT or r.get("event") != "pull_request" or r.get("head_branch") != BRANCH:
            continue
        key = (str(r.get("created_at") or ""), rid)
        conclusion = r.get("conclusion") if type(r.get("conclusion")) is str else ""
        if best is None or key > best[0]:
            best = (key, (rid, attempt, r["status"], conclusion))
    return best[1] if best else None


def oracle_verdict_ok(plan: dict, raw: bytes) -> bool:
    try:
        doc = json.loads(raw.decode("utf-8"))
    except Exception:
        return False
    if type(doc) is not dict or doc.get("run_id") != RUN_ID or doc.get("run_attempt") != ATTEMPT:
        return False
    if type(doc.get("run_id")) is not int or type(doc.get("run_attempt")) is not int:
        return False
    if doc.get("coverage") != "success" or doc.get("contract") != "success":
        return False
    needs, stranded = doc.get("needs"), doc.get("stranded")
    if type(needs) is not dict or type(stranded) is not list:
        return False
    if any(type(s) is not str or s not in OS_LEG_JOBS for s in stranded):
        return False
    for value in needs.values():
        if type(value) is not dict or type(value.get("result")) is not str:
            return False
    expected = expected_jobs(plan)
    if set(needs) != set(expected) | {"plan"} or needs["plan"]["result"] != "success":
        return False
    for job, on in expected.items():
        if job == "ui-visual":
            continue
        if needs[job]["result"] != ("success" if on else "skipped"):
            return False
    return True


PLAN = parent_plan()
PARENT_RUN = cc.ParentRun(RUN_ID, ATTEMPT, "completed", "failure")


def check_against_oracles(case: unittest.TestCase, raw: bytes, label: str) -> None:
    case.assertEqual(cc.lock_only_parent(raw), oracle_parent(raw), label)
    picked = cc.newest_run(raw, PARENT, REPO, BRANCH)
    got = None if picked is None else (picked.run_id, picked.run_attempt, picked.status, picked.conclusion)
    case.assertEqual(got, oracle_newest(raw), label)
    try:
        verdict = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError, RecursionError):
        verdict = None
    problem = cc.ci_verdict_problem(PLAN, verdict, PARENT_RUN, PARENT)
    case.assertEqual(problem is None, oracle_verdict_ok(PLAN, raw), f"{label}: {problem}")


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus: ``commit__``, ``runs__`` and ``verdict__`` files."""

    def test_corpus_matches_oracles(self) -> None:
        seeds = sorted(CORPUS.iterdir())
        self.assertGreaterEqual(len(seeds), 12)
        for seed in seeds:
            with self.subTest(seed=seed.name):
                self.assertIn(seed.name.split("__")[0], ("commit", "runs", "verdict"))
                check_against_oracles(self, seed.read_bytes(), seed.name)

    def test_corpus_has_accepted_and_rejected_cases(self) -> None:
        raw = {p.name: p.read_bytes() for p in CORPUS.iterdir()}
        self.assertEqual(cc.lock_only_parent(raw["commit__approval.json"]), PARENT)
        for name in ("merge", "lock_and_code", "renamed", "added", "odd_types"):
            self.assertIsNone(cc.lock_only_parent(raw[f"commit__{name}.json"]), name)
        self.assertEqual(cc.newest_run(raw["runs__parent.json"], PARENT, REPO, BRANCH).run_id, 11)
        self.assertEqual(cc.newest_run(raw["runs__newer_in_progress.json"], PARENT, REPO, BRANCH).status, "in_progress")
        self.assertIsNone(cc.newest_run(raw["runs__fork.json"], PARENT, REPO, BRANCH))
        self.assertIsNone(cc.newest_run(raw["runs__odd_types.json"], PARENT, REPO, BRANCH))
        for name, ok in (
            ("visual_failed", True),
            ("visual_passed", True),
            ("test_failed", False),
            ("coverage_failed", False),
            ("other_attempt", False),
            ("stranded", False),
        ):
            verdict = json.loads(raw[f"verdict__{name}.json"])
            self.assertEqual(cc.ci_verdict_problem(PLAN, verdict, PARENT_RUN, PARENT) is None, ok, name)


class FuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing: never raises, always agrees with the oracles."""

    VALUES: list[object] = [
        None, True, 0, 1, -1, RUN_ID, 1.5, "", "x", "success", "failure", "skipped", "modified",
        "completed", "pull_request", "macos", LOCK, PARENT, BRANCH, REPO, [], {}, [1], {"result": "success"},
    ]  # fmt: skip

    def mutate_json(self, rng: random.Random, node: object) -> object:
        if isinstance(node, dict) and node and rng.random() < 0.7:
            key = rng.choice(list(node))
            out = dict(node)
            if rng.random() < 0.2:
                del out[key]
            else:
                out[key] = self.mutate_json(rng, node[key])
            return out
        if isinstance(node, list) and node and rng.random() < 0.7:
            out = list(node)
            i = rng.randrange(len(out))
            out[i] = self.mutate_json(rng, out[i])
            if rng.random() < 0.2:
                out.append(copy.deepcopy(out[i]))
            return out
        return rng.choice(self.VALUES)

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
        rng = random.Random(20261123)
        seeds = [json.loads(p.read_bytes()) for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            node = rng.choice(seeds)
            for _ in range(rng.randint(1, 3)):
                node = self.mutate_json(rng, node)
            raw = json.dumps(node).encode()
            check_against_oracles(self, raw, f"iteration {i}: {raw!r}")

    def test_byte_mutations(self) -> None:
        rng = random.Random(1345)
        seeds = [p.read_bytes() for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            raw = self.mutate_bytes(rng, rng.choice(seeds))
            check_against_oracles(self, raw, f"iteration {i}: {raw!r}")


if __name__ == "__main__":
    unittest.main()
