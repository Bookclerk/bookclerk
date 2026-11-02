#!/usr/bin/env python3
"""Reusing main's Rust CodeQL results: run selection, SARIF check and CLI.

``scripts/codeql_reuse.py`` may upload main's Rust SARIF for a pull request
only when it is exactly the analysis of the pull request's base commit.
These tests drive its sans-IO selection with GitHub API bodies and its
command line with a fake ``gh``.
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
BASE = "a" * 40
OTHER = "b" * 40


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("codeql_reuse", ROOT / "scripts" / "codeql_reuse.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["codeql_reuse"] = module
    spec.loader.exec_module(module)
    return module


cr = _load()


def run(run_id: int, **over: object) -> dict:
    """A workflow run that qualifies unless a field is overridden."""
    doc: dict = {
        "id": run_id,
        "head_sha": BASE,
        "head_branch": "main",
        "event": "push",
        "status": "completed",
        "created_at": f"2026-11-02T10:00:{run_id:02d}Z",
    }
    doc.update(over)
    return doc


def runs_body(*runs: dict) -> bytes:
    return json.dumps({"total_count": len(runs), "workflow_runs": list(runs)}).encode()


def jobs_body(rust: str | None = "success", extra: Sequence[dict] = ()) -> bytes:
    jobs = [{"name": "Analyze (python)", "conclusion": "success"}, *extra]
    if rust is not None:
        jobs.append({"name": "Analyze (rust)", "conclusion": rust})
    return json.dumps({"total_count": len(jobs), "jobs": jobs}).encode()


def sarif(**run_over: object) -> bytes:
    one: dict = {"tool": {"driver": {"name": "CodeQL"}}, "automationDetails": {"id": "/language:rust/"}, "results": []}
    one.update(run_over)
    return json.dumps({"version": "2.1.0", "runs": [one]}).encode()


class CandidateRunTests(unittest.TestCase):
    """Only completed main runs of the exact base commit from trusted events."""

    def test_qualifying_runs_newest_first(self) -> None:
        self.assertEqual(cr.candidate_runs(runs_body(run(1), run(3), run(2)), BASE), [3, 2, 1])

    def test_trusted_events(self) -> None:
        for event in ("push", "schedule", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertEqual(cr.candidate_runs(runs_body(run(1, event=event)), BASE), [1])

    def test_rejects_everything_else(self) -> None:
        cases = {
            "pull request run": {"event": "pull_request"},
            "pull_request_target run": {"event": "pull_request_target"},
            "merge queue run": {"event": "merge_group"},
            "other commit": {"head_sha": OTHER},
            "short commit": {"head_sha": BASE[:12]},
            "other branch": {"head_branch": "feature"},
            "still running": {"status": "in_progress"},
            "string id": {"id": "1"},
            "bool id": {"id": True},
        }
        for name, over in cases.items():
            with self.subTest(case=name):
                self.assertEqual(cr.candidate_runs(runs_body(run(1, **over)), BASE), [])

    def test_malformed_bodies(self) -> None:
        for raw in (b"", b"not json", b"\xff", b"[]", b'{"workflow_runs": {}}', b'{"workflow_runs": [1, "x"]}'):
            with self.subTest(raw=raw):
                self.assertEqual(cr.candidate_runs(raw, BASE), [])


class RustJobTests(unittest.TestCase):
    """The run's own Rust analysis must have succeeded."""

    def test_success(self) -> None:
        self.assertTrue(cr.rust_job_succeeded(jobs_body("success")))

    def test_not_success(self) -> None:
        for conclusion in ("failure", "cancelled", "skipped", None):
            with self.subTest(conclusion=conclusion):
                self.assertFalse(cr.rust_job_succeeded(jobs_body(conclusion or "")))

    def test_missing_or_duplicated(self) -> None:
        self.assertFalse(cr.rust_job_succeeded(jobs_body(None)))
        dup = jobs_body("success", extra=[{"name": "Analyze (rust)", "conclusion": "success"}])
        self.assertFalse(cr.rust_job_succeeded(dup))

    def test_malformed_bodies(self) -> None:
        for raw in (b"", b"{", b"\xff", b"[]", b'{"jobs": "x"}'):
            with self.subTest(raw=raw):
                self.assertFalse(cr.rust_job_succeeded(raw))


class SarifTests(unittest.TestCase):
    """Only a CodeQL Rust SARIF is uploaded again."""

    def test_valid(self) -> None:
        self.assertIsNone(cr.sarif_problem(sarif()))
        self.assertIsNone(cr.sarif_problem(sarif(automationDetails=None)))

    def test_problems(self) -> None:
        cases = {
            "not json": b"{",
            "not utf-8": b"\xff",
            "array": b"[]",
            "old version": json.dumps({"version": "2.0.0", "runs": [{}]}).encode(),
            "no runs": json.dumps({"version": "2.1.0", "runs": []}).encode(),
            "run not object": json.dumps({"version": "2.1.0", "runs": [1]}).encode(),
            "other tool": sarif(tool={"driver": {"name": "Semgrep"}}),
            "no driver": sarif(tool={}),
            "python category": sarif(automationDetails={"id": "/language:python/"}),
            "bad details": sarif(automationDetails="rust"),
        }
        for name, raw in cases.items():
            with self.subTest(case=name):
                self.assertIsNotNone(cr.sarif_problem(raw))


class FakeGh:
    """Answers `gh` calls from canned bodies and writes downloaded artifacts."""

    def __init__(self, runs: bytes, jobs: dict[int, bytes], artifacts: dict[int, dict[str, bytes]]) -> None:
        self.runs = runs
        self.jobs = jobs
        self.artifacts = artifacts
        self.calls: list[list[str]] = []

    def __call__(self, args: Sequence[str]) -> bytes:
        args = list(args)
        self.calls.append(args)
        if args[0] == "api" and args[3].endswith("/actions/workflows/codeql.yml/runs"):
            self.assertion_query(args)
            return self.runs
        if args[0] == "api" and args[3].endswith("/jobs"):
            run_id = int(args[3].split("/")[-2])
            if run_id not in self.jobs:
                raise subprocess.CalledProcessError(1, "gh")
            return self.jobs[run_id]
        if args[:2] == ["run", "download"]:
            run_id = int(args[2])
            if run_id not in self.artifacts:
                raise subprocess.CalledProcessError(1, "gh")
            target = Path(args[args.index("-D") + 1])
            target.mkdir(parents=True, exist_ok=True)
            for name, body in self.artifacts[run_id].items():
                (target / name).write_bytes(body)
            return b""
        raise AssertionError(args)

    @staticmethod
    def assertion_query(args: list[str]) -> None:
        assert f"head_sha={BASE}" in args and "branch=main" in args, args


class FindTests(unittest.TestCase):
    """End to end over a fake `gh`."""

    def find(self, gh: FakeGh, base: str = BASE) -> tuple[Path | None, str]:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        return cr.find("Bookclerk/bookclerk", base, Path(self.tmp.name), gh)

    def test_reuses_the_newest_run_whose_rust_job_succeeded(self) -> None:
        gh = FakeGh(
            runs_body(run(1), run(2)),
            {2: jobs_body("failure"), 1: jobs_body("success")},
            {1: {"rust.sarif": sarif()}},
        )
        path, reason = self.find(gh)
        self.assertIsNotNone(path)
        assert path is not None
        self.assertEqual(path.name, "rust.sarif")
        self.assertIn("run 1", reason)
        self.assertIn(["run", "download", "1", "-R", "Bookclerk/bookclerk", "-n", "codeql-rust-sarif"], [c[:7] for c in gh.calls])

    def test_bad_base_never_calls_github(self) -> None:
        gh = FakeGh(runs_body(), {}, {})
        for base in ("", "HEAD", BASE[:12], BASE.upper(), BASE + "\n"):
            with self.subTest(base=base):
                path, _ = self.find(gh, base)
                self.assertIsNone(path)
        self.assertEqual(gh.calls, [])

    def test_falls_back(self) -> None:
        cases = {
            "no runs": FakeGh(runs_body(), {}, {}),
            "jobs call fails": FakeGh(runs_body(run(1)), {}, {}),
            "rust failed": FakeGh(runs_body(run(1)), {1: jobs_body("failure")}, {1: {"rust.sarif": sarif()}}),
            "no artifact": FakeGh(runs_body(run(1)), {1: jobs_body()}, {}),
            "empty artifact": FakeGh(runs_body(run(1)), {1: jobs_body()}, {1: {}}),
            "two files": FakeGh(runs_body(run(1)), {1: jobs_body()}, {1: {"a.sarif": sarif(), "b.sarif": sarif()}}),
            "bad sarif": FakeGh(runs_body(run(1)), {1: jobs_body()}, {1: {"rust.sarif": b"{}"}}),
        }
        for name, gh in cases.items():
            with self.subTest(case=name):
                path, reason = self.find(gh)
                self.assertIsNone(path)
                self.assertTrue(reason)

    def test_listing_failure_falls_back(self) -> None:
        def broken(args: Sequence[str]) -> bytes:
            raise subprocess.CalledProcessError(1, "gh")

        path, reason = self.find(broken)  # type: ignore[arg-type]
        self.assertIsNone(path)
        self.assertIn("failed", reason)


class CliTests(unittest.TestCase):
    """The command line writes `sarif=` only when it reuses."""

    def run_main(self, gh: FakeGh) -> tuple[str, str]:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            summary = Path(tmp) / "summary"
            out.touch()
            old = {k: os.environ.get(k) for k in ("GITHUB_OUTPUT", "GITHUB_STEP_SUMMARY")}
            os.environ["GITHUB_OUTPUT"] = str(out)
            os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
            try:
                args = ["--repo", "Bookclerk/bookclerk", "--base", BASE, "--dest", str(Path(tmp) / "dl")]
                self.assertEqual(cr.main(args, gh=gh), 0)
            finally:
                for k, v in old.items():
                    if v is None:
                        os.environ.pop(k, None)
                    else:
                        os.environ[k] = v
            return out.read_text(encoding="utf-8"), summary.read_text(encoding="utf-8")

    def test_reuse_writes_the_path(self) -> None:
        out, summary = self.run_main(FakeGh(runs_body(run(1)), {1: jobs_body()}, {1: {"rust.sarif": sarif()}}))
        self.assertRegex(out, r"^sarif=.+/1/rust\.sarif\n$")
        self.assertIn("reused", summary)

    def test_fallback_writes_nothing(self) -> None:
        out, summary = self.run_main(FakeGh(runs_body(), {}, {}))
        self.assertEqual(out, "")
        self.assertIn("Rust analyzed:", summary)


if __name__ == "__main__":
    unittest.main()
