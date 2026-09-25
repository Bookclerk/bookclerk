#!/usr/bin/env python3
"""Contract between .github/workflows/ci.yml and the planner/executor.

Parses the workflow with PyYAML (scripts/tests/requirements.txt) so job and
step structure is read, not pattern-matched. Fails when the workflow and the
planner could diverge: an output one side does not know, a check with no
step (or two), a job the gate does not see, a job that skips validation,
or a step that could hide a failure.
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

import yaml

SCRIPTS = Path(__file__).resolve().parents[1]
REPO = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

from ci_plan.plan import CHECK_JOB, JOB_CHECKS  # noqa: E402

WORKFLOW = REPO / ".github" / "workflows" / "ci.yml"
OUTPUT_REF = re.compile(r"needs\.plan\.outputs\.([A-Za-z0-9_]+)")
RUN_CHECK = re.compile(r"ci-exec\.py run ([a-z0-9_]+)\b")


def _load() -> dict:
    # PyYAML (YAML 1.1) reads a bare `on:` key as boolean True.
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


def _emitted_keys() -> set[str]:
    keys = {"mode", "check_needs_cargo", "check_needs_node", "release_mode"}
    keys |= {f"job_{j.replace('-', '_')}" for j in JOB_CHECKS}
    keys |= {f"check_{c}" for c in JOB_CHECKS["check"]}
    return keys


def _strings(node) -> list[str]:
    if isinstance(node, str):
        return [node]
    if isinstance(node, dict):
        return [s for v in node.values() for s in _strings(v)]
    if isinstance(node, list):
        return [s for v in node for s in _strings(v)]
    return []


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.wf = _load()
        cls.jobs = cls.wf["jobs"]

    def test_emitted_outputs_match_plan_job_outputs(self) -> None:
        from ci_plan.execute import job_outputs
        from ci_plan.plan import load_relations, package_index_from_metadata, plan_to_dict
        import json

        meta = json.loads((SCRIPTS / "tests/ci_plan_fixtures/workspace_metadata.json").read_text())
        from ci_plan.plan import build_plan, discover_guests

        index = package_index_from_metadata(meta)
        plan = build_plan(["docs/x.md"], index, load_relations(), guests=discover_guests(REPO, index))
        art = {"mode": "selective", "execution": plan_to_dict(plan)}
        self.assertEqual(set(job_outputs(art)), _emitted_keys())

        declared = self.jobs["plan"]["outputs"]
        self.assertEqual(set(declared), _emitted_keys(), "plan job outputs != ci-exec resolve outputs")
        for key, expr in declared.items():
            self.assertEqual(expr, "${{ steps.resolve.outputs.%s }}" % key)

    def test_every_referenced_output_is_emitted_and_every_output_is_used(self) -> None:
        used: set[str] = set()
        for name, job in self.jobs.items():
            if name == "plan":
                continue
            for s in _strings(job):
                used |= set(OUTPUT_REF.findall(s))
        self.assertLessEqual(used, _emitted_keys(), f"unknown outputs: {used - _emitted_keys()}")
        # `mode` is for humans (summary / logs); everything else gates work.
        self.assertEqual(_emitted_keys() - used, {"mode"})

    def test_jobs_match_planner(self) -> None:
        gated = [j for j in self.jobs if j not in ("plan", "ci-gate")]
        self.assertEqual(set(gated), set(JOB_CHECKS))
        for job in gated:
            self.assertEqual(
                self.jobs[job].get("if"),
                f"needs.plan.outputs.job_{job.replace('-', '_')} == 'true'",
                job,
            )
            self.assertEqual(self.jobs[job].get("needs"), "plan", job)

    def test_each_check_has_exactly_one_step_in_its_job(self) -> None:
        seen: dict[str, str] = {}
        for job_name, job in self.jobs.items():
            for step in job.get("steps", []):
                for check in RUN_CHECK.findall(step.get("run", "")):
                    self.assertNotIn(check, seen, f"{check} run in {seen.get(check)} and {job_name}")
                    self.assertEqual(CHECK_JOB.get(check), job_name, f"{check} runs in wrong job {job_name}")
                    seen[check] = job_name
                    if job_name == "check":
                        self.assertEqual(step.get("if"), f"needs.plan.outputs.check_{check} == 'true'")
        self.assertEqual(set(seen), set(CHECK_JOB))

    def test_every_downstream_job_downloads_and_validates_first(self) -> None:
        for name, job in self.jobs.items():
            if name == "plan":
                continue
            steps = job["steps"]
            download = [i for i, s in enumerate(steps) if str(s.get("uses", "")).startswith("actions/download-artifact@")]
            self.assertEqual(len(download), 1, name)
            self.assertEqual(steps[download[0]]["with"]["name"], "ci-plan", name)
            self.assertEqual(steps[download[0]]["with"]["path"], "${{ runner.temp }}/ci-plan", name)
            execs = [i for i, s in enumerate(steps) if "ci-exec.py" in s.get("run", "")]
            self.assertTrue(execs, name)
            self.assertIn("ci-exec.py validate", steps[execs[0]]["run"], f"{name}: validate must be the first ci-exec call")
            self.assertLess(download[0], execs[0], name)

    def test_plan_job_uploads_the_resolved_artifact(self) -> None:
        steps = self.jobs["plan"]["steps"]
        resolve = [s for s in steps if "ci-exec.py resolve" in s.get("run", "")]
        self.assertEqual(len(resolve), 1)
        self.assertEqual(resolve[0].get("id"), "resolve")
        self.assertIn('--selective "$SELECTIVE_CI"', resolve[0]["run"])
        upload = [s for s in steps if str(s.get("uses", "")).startswith("actions/upload-artifact@")]
        self.assertEqual(len(upload), 1)
        self.assertEqual(upload[0]["with"]["path"], "${{ runner.temp }}/ci-plan/ci-plan.json")
        self.assertEqual(upload[0]["with"]["if-no-files-found"], "error")

    def test_full_suite_forced_on_main_and_merge_group(self) -> None:
        run = next(s["run"] for s in self.jobs["plan"]["steps"] if "ci-exec.py resolve" in s.get("run", ""))
        self.assertIn('"${{ github.event_name }}" = "merge_group"', run)
        self.assertIn('"${{ github.ref }}" = "refs/heads/main"', run)
        self.assertIn("--force-full", run)
        triggers = self.wf.get("on", self.wf.get(True))
        self.assertIn("merge_group", triggers)
        self.assertEqual(triggers["push"]["branches"], ["main"])

    def test_gate_sees_every_job_and_runs_always(self) -> None:
        gate = self.jobs["ci-gate"]
        self.assertEqual(gate["name"], "CI Gate")
        self.assertEqual(gate["if"], "always()")
        self.assertEqual(set(gate["needs"]), {"plan", *JOB_CHECKS})
        run = gate["steps"][-1]
        self.assertIn("ci-exec.py gate", run["run"])
        self.assertEqual(run["env"]["NEEDS_JSON"], "${{ toJSON(needs) }}")

    def test_native_gateway_runs_on_every_os(self) -> None:
        job = self.jobs["native-gateway"]
        self.assertEqual(job["runs-on"], "${{ matrix.os }}")
        self.assertIs(job["strategy"]["fail-fast"], False)
        self.assertEqual(job["strategy"]["matrix"], {"os": ["ubuntu-latest", "macos-latest", "windows-latest"]})
        steps = job["steps"]
        capnp = {s.get("if") for s in steps if "capnp" in s.get("run", "") and "ci-exec.py" not in s.get("run", "")}
        self.assertEqual(
            capnp, {"runner.os == 'Linux'", "runner.os == 'macOS'", "runner.os == 'Windows'"}
        )
        run = [s for s in steps if "ci-exec.py run native_gateway" in s.get("run", "")]
        self.assertEqual(len(run), 1)
        self.assertNotIn("if", run[0], "the smoke must run on every matrix child")
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertNotIn("windows_gateway", text)
        self.assertNotIn("windows-gateway", text)

    def test_native_gateway_smoke_inputs_exist(self) -> None:
        from ci_plan.plan import load_relations

        for pattern in load_relations().native_gateway_paths:
            if "*" not in pattern:
                self.assertTrue((REPO / pattern).is_file(), pattern)

    def test_nothing_can_hide_a_failure(self) -> None:
        for name, job in self.jobs.items():
            self.assertNotIn("continue-on-error", job, name)
            for step in job.get("steps", []):
                self.assertNotIn("continue-on-error", step, f"{name}: {step.get('name')}")

    def test_selective_ci_default_is_explicit(self) -> None:
        self.assertIn(self.wf["env"]["SELECTIVE_CI"], ("0", "1"))

    def test_actions_are_pinned(self) -> None:
        for name, job in self.jobs.items():
            for step in job.get("steps", []):
                uses = step.get("uses")
                if uses:
                    self.assertRegex(uses, r"@[0-9a-f]{40}$", f"{name}: {uses}")


if __name__ == "__main__":
    unittest.main()
