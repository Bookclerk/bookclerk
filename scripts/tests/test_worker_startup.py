#!/usr/bin/env python3
"""Tests for scripts/worker_startup.py: profiles, deploy output, budgets and wiring."""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("worker_startup", ROOT / "scripts" / "worker_startup.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["worker_startup"] = module
    spec.loader.exec_module(module)
    return module


ws = _load()


def profile(samples: list[int], deltas: list[float]) -> str:
    """A minimal ``.cpuprofile``: node 1 is idle, 2 garbage collection, 3 code."""
    nodes = [
        {"id": 1, "callFrame": {"functionName": "(idle)"}},
        {"id": 2, "callFrame": {"functionName": "(garbage collector)"}},
        {"id": 3, "callFrame": {"functionName": "render"}},
    ]
    return json.dumps({"nodes": nodes, "samples": samples, "timeDeltas": deltas, "startTime": 0, "endTime": 9})


CONFIG = """
mode = "report"
runs = 3
deploy_warn_ms = 500

[workers.site]
path = "tools/site"
limit_ms = 100

[workers.demo]
path = "tools/demo"
limit_ms = 50
extra_args = ["--containers-rollout=none"]
"""


class ProfileTest(unittest.TestCase):
    """``summarize_profile`` matches wrangler's own summary."""

    def test_active_time_leaves_out_idle(self) -> None:
        summary = ws.summarize_profile(profile([3, 1, 2, 3], [4000, 10000, 2000, 6000]))
        self.assertEqual(summary, ws.Startup(active_ms=12.0, gc_ms=2.0, samples=4))

    def test_unknown_sample_counts_as_active(self) -> None:
        summary = ws.summarize_profile(profile([9], [1500]))
        self.assertEqual(summary, ws.Startup(active_ms=1.5, gc_ms=0.0, samples=1))

    def test_unusable_profiles(self) -> None:
        for text in ("", "[]", "{}", '{"nodes": [{}]}', profile([3], [-1]), profile([3], [float("nan")]), profile([3], ["1"])):
            with self.subTest(text=text):
                self.assertIsNone(ws.summarize_profile(text))

    def test_empty_profile_is_zero(self) -> None:
        self.assertEqual(ws.summarize_profile(profile([], [])), ws.Startup(0.0, 0.0, 0))


class DeployOutputTest(unittest.TestCase):
    """``parse_deploy_output`` finds the figure wrangler prints."""

    def test_reads_the_last_figure(self) -> None:
        text = "Total Upload: 5.6 MiB\nWorker Startup Time: 12 ms\nUploaded bookclerk-site\nWorker Startup Time: 31.5 ms\n"
        self.assertEqual(ws.parse_deploy_output(text), 31.5)

    def test_absent_or_malformed(self) -> None:
        for text in ("", "Worker Startup Time: ms", "Worker Startup Time: -3 ms", "Startup: 12 ms"):
            with self.subTest(text=text):
                self.assertIsNone(ws.parse_deploy_output(text))


class ConfigTest(unittest.TestCase):
    """The budgets file parses and rejects bad shapes."""

    def test_parses(self) -> None:
        config = ws.parse_config(CONFIG)
        self.assertFalse(config.enforce)
        self.assertEqual(config.runs, 3)
        self.assertEqual(config.workers["demo"].extra_args, ("--containers-rollout=none",))

    def test_rejects(self) -> None:
        for bad in (
            CONFIG.replace('"report"', '"strict"'),
            CONFIG.replace("runs = 3", "runs = 0"),
            CONFIG.replace("limit_ms = 100", "limit_ms = 2000"),
            CONFIG.replace("deploy_warn_ms = 500", ""),
            CONFIG.replace('["--containers-rollout=none"]', "[1]"),
            "not toml [",
        ):
            with self.subTest(bad=bad[:40]), self.assertRaises(ws.ConfigError):
                ws.parse_config(bad)

    def test_repository_file(self) -> None:
        config = ws.parse_config((ROOT / ws.CONFIG_FILE).read_text(encoding="utf-8"))
        self.assertEqual(set(config.workers), {"site", "collector", "demo"})
        for worker in config.workers.values():
            self.assertTrue((ROOT / worker.path / "wrangler.toml").is_file() or (ROOT / worker.path / "wrangler.jsonc").is_file(), worker.path)


class RenderTest(unittest.TestCase):
    """The summary reports the median, so one cold start does not count."""

    def test_median_and_verdict(self) -> None:
        worker = ws.Worker("site", "tools/site", 100)
        runs = [ws.Startup(400.0, 0, 9), ws.Startup(30.0, 0, 9), ws.Startup(32.0, 0, 9)]
        markdown, median = ws.render_profiles(worker, runs)
        self.assertEqual(median, 32.0)
        self.assertIn("within budget", markdown)
        _, over = ws.render_profiles(worker, [ws.Startup(150.0, 0, 9)])
        self.assertEqual(over, 150.0)


class DeployTest(unittest.TestCase):
    """A deploy warns past the warning level and never fails."""

    def run_deploy(self, output: str) -> str:
        config = ws.parse_config(CONFIG)
        with tempfile.NamedTemporaryFile("r", suffix=".md") as summary, mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": summary.name}):
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(ws.deploy(config, config.workers["site"], output), 0)
            printed = "".join(call.args[0] for call in stdout.write.call_args_list)
            return printed + summary.read()

    def test_warns_near_the_limit(self) -> None:
        self.assertIn("::warning", self.run_deploy("Worker Startup Time: 640 ms"))
        quiet = self.run_deploy("Worker Startup Time: 40 ms")
        self.assertNotIn("::warning", quiet)
        self.assertIn("**40 ms**", quiet)
        self.assertIn("::notice", self.run_deploy("no figure"))


class WiringTest(unittest.TestCase):
    """CI profiles every budgeted Worker, and every deploy reports its figure."""

    def test_deploy_workflows_report_startup(self) -> None:
        import yaml

        for workflow, worker in (("site-deploy", "site"), ("demo-deploy", "demo"), ("diagnostics-collector-deploy", "collector")):
            with self.subTest(workflow=workflow):
                steps = [s for job in yaml.safe_load((ROOT / ".github" / "workflows" / f"{workflow}.yml").read_text(encoding="utf-8"))["jobs"].values() for s in job["steps"]]
                ids = [s.get("id") for s in steps]
                report = next(s for s in steps if s.get("name") == "Report the startup time")
                deploy = steps[ids.index("deploy")]
                self.assertIn("cloudflare/wrangler-action@", deploy["uses"])
                self.assertGreater(steps.index(report), steps.index(deploy))
                self.assertEqual(report["env"]["WRANGLER_OUTPUT"], "${{ steps.deploy.outputs.command-output }}")
                self.assertEqual(report["run"], f"python3 scripts/worker_startup.py deploy --worker {worker}")
                self.assertEqual(report.get("if"), deploy.get("if"))

    def test_ci_profiles_each_worker(self) -> None:
        sys.path.insert(0, str(ROOT / "scripts"))
        from ci_plan import execute

        with mock.patch.dict(os.environ, {execute.WORKER_STARTUP_ENV: "1"}):
            for worker in ("site", "collector", "demo"):
                self.assertEqual(
                    execute._worker_startup(worker)[0].argv, ["python3", "scripts/worker_startup.py", "profile", "--worker", worker]
                )
        with mock.patch.dict(os.environ):
            os.environ.pop(execute.WORKER_STARTUP_ENV, None)
            self.assertEqual(execute._worker_startup("site"), [])


if __name__ == "__main__":
    unittest.main()
