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
import random
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "codeql_reuse_corpus"
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
            "event list": {"event": []},
            "event object": {"event": {"push": 1}},
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
        for conclusion in ("failure", "cancelled", "skipped", "", None):
            with self.subTest(conclusion=conclusion):
                body = json.dumps({"jobs": [{"name": "Analyze (rust)", "conclusion": conclusion}]}).encode()
                self.assertFalse(cr.rust_job_succeeded(body))

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
        self.assertIsNone(cr.sarif_problem(sarif(automationDetails={"id": "/language:rust"})))

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
            "longer category": sarif(automationDetails={"id": "/language:rust-other/"}),
            "nested category": sarif(automationDetails={"id": "/language:rust/other/"}),
            "no details": json.dumps(
                {"version": "2.1.0", "runs": [{"tool": {"driver": {"name": "CodeQL"}}, "results": []}]}
            ).encode(),
            "null details": sarif(automationDetails=None),
            "details without id": sarif(automationDetails={}),
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

    def test_invalid_sarif_tries_an_older_run(self) -> None:
        gh = FakeGh(
            runs_body(run(1), run(2)),
            {1: jobs_body(), 2: jobs_body()},
            {2: {"rust.sarif": b"{}"}, 1: {"rust.sarif": sarif()}},
        )
        path, reason = self.find(gh)
        self.assertIsNotNone(path)
        self.assertIn("run 1", reason)

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


class RunGhTests(unittest.TestCase):
    """A stalled `gh` call falls back instead of holding the job."""

    def test_timeout_becomes_a_handled_failure(self) -> None:
        old = cr.subprocess.run

        def stall(*args: object, **kwargs: object) -> object:
            self.assertEqual(kwargs.get("timeout"), cr.GH_TIMEOUT_SECONDS)
            raise subprocess.TimeoutExpired(cmd=["gh"], timeout=1)

        cr.subprocess.run = stall  # type: ignore[assignment]
        try:
            with self.assertRaises(subprocess.CalledProcessError):
                cr.run_gh(["api", "x"])
        finally:
            cr.subprocess.run = old


# Independent oracles, written with explicit walks instead of the script's
# helpers, for the corpus replay and the mutation fuzzing below.
def oracle_runs(raw: bytes) -> list[int]:
    try:
        doc = json.loads(raw.decode("utf-8"))
    except Exception:
        return []
    if type(doc) is not dict or type(doc.get("workflow_runs")) is not list:
        return []
    keep = []
    for r in doc["workflow_runs"]:
        if type(r) is not dict or type(r.get("id")) is not int:
            continue
        ok = (
            r.get("head_sha") == BASE
            and r.get("head_branch") == "main"
            and r.get("event") in ("push", "schedule", "workflow_dispatch")
            and r.get("status") == "completed"
        )
        if ok:
            keep.append((str(r.get("created_at") or ""), r["id"]))
    return [i for _, i in sorted(keep, reverse=True)]


def oracle_jobs(raw: bytes) -> bool:
    try:
        doc = json.loads(raw.decode("utf-8"))
    except Exception:
        return False
    if type(doc) is not dict or type(doc.get("jobs")) is not list:
        return False
    rust = [j for j in doc["jobs"] if type(j) is dict and j.get("name") == "Analyze (rust)"]
    return len(rust) == 1 and rust[0].get("conclusion") == "success"


def oracle_sarif_ok(raw: bytes) -> bool:
    try:
        doc = json.loads(raw.decode("utf-8"))
    except Exception:
        return False
    if type(doc) is not dict or doc.get("version") != "2.1.0" or type(doc.get("runs")) is not list:
        return False
    if not doc["runs"]:
        return False
    for r in doc["runs"]:
        if type(r) is not dict:
            return False
        tool = r.get("tool")
        driver = tool.get("driver") if type(tool) is dict else None
        if type(driver) is not dict or driver.get("name") != "CodeQL":
            return False
        details = r.get("automationDetails")
        if type(details) is not dict or details.get("id") not in ("/language:rust", "/language:rust/"):
            return False
    return True


def check_against_oracles(case: unittest.TestCase, raw: bytes, label: str) -> None:
    case.assertEqual(cr.candidate_runs(raw, BASE), oracle_runs(raw), label)
    case.assertEqual(cr.rust_job_succeeded(raw), oracle_jobs(raw), label)
    case.assertEqual(cr.sarif_problem(raw) is None, oracle_sarif_ok(raw), label)


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus: ``runs__``, ``jobs__`` and ``sarif__`` files."""

    def test_corpus_matches_oracles(self) -> None:
        seeds = sorted(CORPUS.iterdir())
        self.assertGreaterEqual(len(seeds), 10)
        for seed in seeds:
            with self.subTest(seed=seed.name):
                self.assertIn(seed.name.split("__")[0], ("runs", "jobs", "sarif"))
                check_against_oracles(self, seed.read_bytes(), seed.name)

    def test_corpus_has_accepted_and_rejected_cases(self) -> None:
        raw = {p.name: p.read_bytes() for p in CORPUS.iterdir()}
        self.assertEqual(cr.candidate_runs(raw["runs__two_push.json"], BASE), [2, 1])
        self.assertEqual(cr.candidate_runs(raw["runs__mixed.json"], BASE), [5])
        self.assertEqual(cr.candidate_runs(raw["runs__odd_types.json"], BASE), [])
        self.assertTrue(cr.rust_job_succeeded(raw["jobs__ok.json"]))
        self.assertFalse(cr.rust_job_succeeded(raw["jobs__null.json"]))
        self.assertIsNone(cr.sarif_problem(raw["sarif__rust.sarif"]))
        self.assertIsNotNone(cr.sarif_problem(raw["sarif__no_details.sarif"]))


class FuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing: never raises, always agrees with the oracles."""

    VALUES: list[object] = [None, True, 0, 1, -1, 1.5, "", "x", "push", "main", BASE, [], {}, [1], {"a": 1}]

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
        rng = random.Random(20261102)
        seeds = [json.loads(p.read_bytes()) for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            node = rng.choice(seeds)
            for _ in range(rng.randint(1, 3)):
                node = self.mutate_json(rng, node)
            raw = json.dumps(node).encode()
            check_against_oracles(self, raw, f"iteration {i}: {raw!r}")

    def test_byte_mutations(self) -> None:
        rng = random.Random(586)
        seeds = [p.read_bytes() for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            raw = self.mutate_bytes(rng, rng.choice(seeds))
            check_against_oracles(self, raw, f"iteration {i}: {raw!r}")


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
