#!/usr/bin/env python3
"""contract_coverage.py: declared pairs, hit parsing, the report and its rendering.

Everything runs offline against small documents and report texts; `MainTest`
runs the whole script against a temporary reports directory and the
repository's committed document.
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    path = ROOT / "scripts" / "contract_coverage.py"
    spec = importlib.util.spec_from_file_location("contract_coverage", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


cc = _load()

DOCUMENT = {
    "paths": {
        "/api/things/{id}": {
            "get": {
                "tags": ["Things"],
                "responses": {
                    "200": {"content": {"application/json": {"schema": {}}}},
                    "204": {"description": "gone"},
                    "default": {"content": {"application/json; charset=utf-8": {}}},
                },
            },
            "delete": {"responses": {"204": {}}},
        },
        "/api/other": {"post": {"tags": ["Other"], "responses": {"201": {"content": {"text/plain": {}}}}}},
    }
}


def line(method: str, path: str, status: int, media: str, kind: str, **extra: object) -> str:
    return json.dumps(
        {"method": method, "path": path, "status": status, "media": media, "outcome": {"kind": kind, **extra}}
    )


class DeclaredPairsTest(unittest.TestCase):
    def test_every_response_media_and_default_is_a_pair_with_its_area(self) -> None:
        pairs = cc.declared_pairs(DOCUMENT)
        self.assertEqual(
            pairs,
            {
                ("get", "/api/things/{id}", "200", "application/json"): "Things",
                ("get", "/api/things/{id}", "204", "none"): "Things",
                ("get", "/api/things/{id}", "default", "application/json"): "Things",
                ("delete", "/api/things/{id}", "204", "none"): cc.UNTAGGED,
                ("post", "/api/other", "201", "text/plain"): "Other",
            },
        )

    def test_a_document_without_paths_declares_nothing(self) -> None:
        self.assertEqual(cc.declared_pairs({}), {})
        self.assertEqual(cc.declared_pairs({"paths": {"/x": "nope", "/y": {"get": 1}}}), {})


class ParseHitsTest(unittest.TestCase):
    def test_hits_are_read_and_other_lines_counted(self) -> None:
        text = "\n".join(
            [
                line("get", "/api/things/{id}", 200, "application/json", "declared"),
                line("get", "/api/things/{id}", 500, "application/json", "invalid", errors=["a", "b"]),
                line("get", "/api/things/{id}", 200, "application/json", "unparsed", error="EOF"),
                "",
                "not json",
                json.dumps({"method": "get"}),
                json.dumps({"method": "get", "path": "/x", "status": True, "media": "none", "outcome": {"kind": "declared"}}),
                json.dumps({"method": "get", "path": "/x", "status": 200, "media": "none", "outcome": "declared"}),
                json.dumps([1]),
            ]
        )
        hits, malformed = cc.parse_hits(text)
        self.assertEqual(malformed, 5)
        self.assertEqual([h.kind for h in hits], ["declared", "invalid", "unparsed"])
        self.assertEqual(hits[1].detail, "a; b")
        self.assertEqual(hits[2].detail, "EOF")
        self.assertEqual(hits[0].pair, ("get", "/api/things/{id}", "200", "application/json"))


class BuildReportTest(unittest.TestCase):
    def test_observed_pairs_fall_back_to_default_and_disagreements_dedupe(self) -> None:
        first = "\n".join(
            [
                line("get", "/api/things/{id}", 200, "application/json", "declared"),
                line("get", "/api/things/{id}", 404, "application/json", "declared"),
                line("get", "/api/things/{id}", 200, "text/html", "undeclared_media"),
                line("get", "/api/things/{id}", 200, "text/html", "undeclared_media"),
                line("get", "/invite", 200, "text/html", "undocumented"),
                line("get", "/cluster", 200, "text/html; charset=utf-8", "undocumented"),
                line("get", "/secret", 200, "application/json", "undocumented"),
            ]
        )
        second = "\n".join(
            [
                line("post", "/api/other", 500, "none", "undeclared_status"),
                line("get", "/api/things/{id}", 200, "application/json", "invalid", errors=["x | y"]),
                "garbage",
            ]
        )
        report = cc.build_report(DOCUMENT, [first, second])
        self.assertEqual(report.files, 2)
        self.assertEqual(report.malformed, 1)
        self.assertEqual(
            report.observed,
            {
                ("get", "/api/things/{id}", "200", "application/json"),
                ("get", "/api/things/{id}", "default", "application/json"),
            },
        )
        self.assertEqual(
            [(h.kind, h.path, h.status) for h in report.disagreements],
            [
                ("invalid", "/api/things/{id}", 200),
                ("undeclared_media", "/api/things/{id}", 200),
                ("undeclared_status", "/api/other", 500),
                ("undocumented", "/secret", 200),
            ],
        )
        self.assertEqual(report.by_area(), {"(untagged)": (1, 0), "Other": (1, 0), "Things": (3, 2)})

    def test_render_lists_disagreements_and_areas(self) -> None:
        report = cc.build_report(
            DOCUMENT,
            [line("get", "/api/things/{id}", 200, "application/json", "invalid", errors=["a | b"])],
        )
        text = cc.render(report, "report")
        self.assertIn("**0 of 5** (0.0%)", text)
        self.assertIn("each is reported only", text)
        self.assertIn("| `invalid` | `GET` | `/api/things/{id}` | 200 | `application/json` | a \\| b |", text)
        self.assertIn("| Things | 3 | 0 | 0.0% |", text)
        self.assertLess(text.index("each is reported only"), text.index("<details><summary>Observed responses per area"))
        self.assertTrue(text.rstrip().endswith("</details>"))
        self.assertIn("each fails CI Gate", cc.render(report, "enforce"))
        clean = cc.render(cc.build_report(DOCUMENT, []), "report")
        self.assertIn("Every response the tests produced agrees", clean)
        self.assertEqual(cc.percent(1, 0), "n/a")

    def test_long_lists_are_cut(self) -> None:
        lines = [line("get", f"/r{i}", 200, "none", "undocumented") for i in range(cc.MAX_LISTED + 3)]
        text = cc.render(cc.build_report(DOCUMENT, ["\n".join(lines)]), "report")
        self.assertIn("| … | | | | | 3 more |", text)

    def test_json_output_lists_unobserved_pairs(self) -> None:
        data = cc.as_json(cc.build_report(DOCUMENT, []))
        self.assertEqual(data["declared"], 5)
        self.assertEqual(data["observed"], 0)
        self.assertIn("post /api/other 201 text/plain", data["unobserved"])
        self.assertEqual(data["disagreements"], [])


class FloorTest(unittest.TestCase):
    def test_the_floor_is_read_from_the_contract_table(self) -> None:
        self.assertEqual(cc.read_floor("mode = 'enforce'\n[contract]\nfloor = 8\n"), 8.0)
        self.assertIsNone(cc.read_floor("mode = 'enforce'\n[areas.rust]\nfloor = 74\n"))
        for bad in ("[contract]\nmode = 1\n", "[contract]\nfloor = true\n", "[contract]\nfloor = 101\n", "contract = 3\n", "= nope"):
            with self.assertRaises(ValueError, msg=bad):
                cc.read_floor(bad)

    def test_the_committed_file_states_a_floor(self) -> None:
        text = (ROOT / "scripts" / "coverage_areas.toml").read_text(encoding="utf-8")
        floor = cc.read_floor(text)
        self.assertIsNotNone(floor)
        self.assertGreaterEqual(floor, 8)

    def test_the_floor_needs_report_files_and_counts_the_share(self) -> None:
        document = {"paths": {"/api/jobs": {"get": {"tags": ["Jobs"], "responses": {"200": {"content": {"application/json": {}}}, "404": {}}}}}}
        empty = cc.build_report(document, [])
        self.assertIsNone(cc.share(cc.build_report({"paths": {}}, [])))
        self.assertFalse(cc.below_floor(empty, 50.0))
        self.assertIn("not checked: no report file", cc.render(empty, "enforce", 50.0))
        half = cc.build_report(document, [line("get", "/api/jobs", 200, "application/json", "declared")])
        self.assertEqual(cc.share(half), 50.0)
        self.assertFalse(cc.below_floor(half, 50.0))
        self.assertTrue(cc.below_floor(half, 50.5))
        self.assertFalse(cc.below_floor(half, None))
        self.assertIn("Floor: 50.5% of the declared responses observed (**below the floor**; this fails CI Gate).", cc.render(half, "enforce", 50.5))
        self.assertIn("(**below the floor**; this is reported only)", cc.render(half, "report", 50.5))
        self.assertIn("Floor: 50% of the declared responses observed (met).", cc.render(half, "enforce", 50.0))
        self.assertNotIn("Floor:", cc.render(half, "enforce", None))
        data = cc.as_json(half, 50.5)
        self.assertEqual((data["share"], data["floor"], data["below_floor"]), (50.0, 50.5, True))


class MainTest(unittest.TestCase):
    def run_script(self, *args: str) -> subprocess.CompletedProcess[str]:
        # Without `--summary` the script appends to $GITHUB_STEP_SUMMARY; under
        # CI that is the plan job's own summary, which these runs must not fill
        # with test reports.
        env = {key: value for key, value in os.environ.items() if key != "GITHUB_STEP_SUMMARY"}
        return subprocess.run(
            [sys.executable, "-I", str(ROOT / "scripts" / "contract_coverage.py"), *args],
            capture_output=True,
            text=True,
            cwd=ROOT,
            env=env,
            check=False,
        )

    def test_runs_leave_the_callers_step_summary_alone(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            outer = Path(tmp) / "outer-summary.md"
            outer.write_text("", encoding="utf-8")
            reports = Path(tmp) / "reports"
            reports.mkdir()
            (reports / "1.jsonl").write_text(line("get", "/api/jobs", 200, "application/json", "declared") + "\n", encoding="utf-8")
            previous = os.environ.get("GITHUB_STEP_SUMMARY")
            os.environ["GITHUB_STEP_SUMMARY"] = str(outer)
            try:
                result = self.run_script("--reports", str(reports), "--floor", "0")
            finally:
                if previous is None:
                    del os.environ["GITHUB_STEP_SUMMARY"]
                else:
                    os.environ["GITHUB_STEP_SUMMARY"] = previous
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(outer.read_text(encoding="utf-8"), "")

    def test_report_mode_summarizes_and_enforce_mode_fails_on_a_disagreement(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            reports = Path(tmp) / "reports"
            (reports / "nested").mkdir(parents=True)
            (reports / "nested" / "1.jsonl").write_text(
                line("get", "/api/jobs", 200, "application/json", "declared") + "\n", encoding="utf-8"
            )
            (reports / "2.jsonl").write_text(
                line("get", "/api/jobs", 418, "application/json", "undeclared_status") + "\n", encoding="utf-8"
            )
            summary = Path(tmp) / "summary.md"
            result = self.run_script("--reports", str(reports), "--summary", str(summary))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("## Web API contract", summary.read_text(encoding="utf-8"))
            self.assertIn("`undeclared_status`", result.stdout)
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--format", "json")
            self.assertEqual(result.returncode, 1, result.stderr)
            data = json.loads(result.stdout)
            self.assertEqual(data["observed"], 1)
            self.assertEqual(data["files"], 2)
            result = self.run_script("--reports", str(Path(tmp) / "missing"), "--format", "json")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["files"], 0)

    def test_a_run_without_the_daemons_tests_leaves_the_summary_alone(self) -> None:
        workspace = json.dumps({"execution": {"checks": {"rust_test": {"params": {"workspace": True}}}}})
        build_only = json.dumps({"build_only": True, "execution": {"checks": {"rust_test": {"params": {"workspace": True}}}}})
        with tempfile.TemporaryDirectory() as tmp:
            plan = Path(tmp) / "ci-plan.json"
            summary = Path(tmp) / "summary.md"
            empty = Path(tmp) / "reports"
            empty.mkdir()
            plan.write_text(build_only, encoding="utf-8")
            result = self.run_script("--reports", str(empty), "--mode", "enforce", "--plan", str(plan), "--summary", str(summary))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "Web API contract: not checked; the daemon's tests did not run in this plan.\n")
            self.assertFalse(summary.exists())
            plan.write_text(workspace, encoding="utf-8")
            for ended in ("cancelled", "failure", "skipped"):
                result = self.run_script(
                    "--reports", str(empty), "--mode", "enforce", "--plan", str(plan), "--tests-result", ended, "--summary", str(summary)
                )
                self.assertEqual(result.returncode, 0, ended)
                self.assertIn(f"the test job ended `{ended}` without a report", result.stdout)
                self.assertFalse(summary.exists())
            result = self.run_script("--reports", str(empty), "--mode", "enforce", "--plan", str(plan), "--tests-result", "success", "--summary", str(summary))
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("**No report file**", summary.read_text(encoding="utf-8"))
            result = self.run_script("--reports", str(empty), "--tests-result", "neutral")
            self.assertEqual(result.returncode, 2)
            (empty / "1.jsonl").write_text(line("get", "/api/jobs", 200, "application/json", "declared") + "\n", encoding="utf-8")
            summary.unlink()
            result = self.run_script("--reports", str(empty), "--mode", "enforce", "--floor", "0", "--tests-result", "failure", "--summary", str(summary))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("## Web API contract", summary.read_text(encoding="utf-8"))

    def test_enforce_mode_applies_the_floor_from_the_option_or_the_config(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            reports = Path(tmp) / "reports"
            reports.mkdir()
            (reports / "1.jsonl").write_text(
                line("get", "/api/jobs", 200, "application/json", "declared") + "\n", encoding="utf-8"
            )
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--floor", "99", "--format", "json")
            self.assertEqual(result.returncode, 1, result.stderr)
            data = json.loads(result.stdout)
            self.assertEqual((data["floor"], data["below_floor"], data["disagreements"]), (99.0, True, []))
            result = self.run_script("--reports", str(reports), "--mode", "report", "--floor", "99")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("below the floor", result.stdout)
            config = Path(tmp) / "areas.toml"
            config.write_text("[contract]\nfloor = 0\n", encoding="utf-8")
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--config", str(config), "--format", "json")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["floor"], 0.0)
            config.write_text("[contract]\nfloor = 'nine'\n", encoding="utf-8")
            result = self.run_script("--reports", str(reports), "--config", str(config))
            self.assertEqual(result.returncode, 2)
            self.assertIn("floor must be a percentage", result.stderr)
            result = self.run_script("--reports", str(Path(tmp) / "missing"), "--mode", "enforce", "--floor", "99", "--format", "json")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(json.loads(result.stdout)["below_floor"])
            for bad in ("nan", "inf", "-1", "101", "nine"):
                result = self.run_script("--reports", str(reports), "--mode", "enforce", "--floor", bad)
                self.assertEqual(result.returncode, 2, bad)
                self.assertIn("--floor", result.stderr)
            result = self.run_script("--reports", str(reports), "--config", str(Path(tmp) / "absent.toml"))
            self.assertEqual(result.returncode, 2)
            self.assertIn("absent.toml", result.stderr)

    def test_enforce_mode_demands_the_report_the_plan_promises_and_whole_lines(self) -> None:
        daemon = json.dumps({"execution": {"checks": {"rust_test": {"params": {"packages": ["bookclerkd"]}}}}})
        workspace = json.dumps({"execution": {"checks": {"rust_test": {"params": {"workspace": True}}}}})
        other = json.dumps({"execution": {"checks": {"rust_test": {"params": {"packages": ["bookclerk-mp4"]}}}}})
        build_only = json.dumps({"build_only": True, "execution": {"checks": {"rust_test": {"params": {"workspace": True}}}}})
        for text, expected in ((daemon, True), (workspace, True), (other, False), (build_only, False), ("{}", False), ("nope", False), ('{"execution": {"checks": {"rust_test": {"params": 3}}}}', False)):
            self.assertEqual(cc.plan_tested_daemon(text), expected, text)
        with tempfile.TemporaryDirectory() as tmp:
            plan = Path(tmp) / "ci-plan.json"
            plan.write_text(daemon, encoding="utf-8")
            reports = Path(tmp) / "reports"
            reports.mkdir()
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--floor", "0", "--plan", str(plan), "--format", "json")
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertTrue(json.loads(result.stdout)["missing"])
            result = self.run_script("--reports", str(reports), "--mode", "report", "--floor", "0", "--plan", str(plan))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("**No report file**, though the plan ran the daemon's tests; this is reported only.", result.stdout)
            plan.write_text(other, encoding="utf-8")
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--floor", "0", "--plan", str(plan), "--format", "json")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(json.loads(result.stdout)["missing"])
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--plan", str(Path(tmp) / "absent.json"))
            self.assertEqual(result.returncode, 2)
            (reports / "1.jsonl").write_text(
                line("get", "/api/jobs", 200, "application/json", "declared") + "\n" + '{"method": "get", "pa', encoding="utf-8"
            )
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--floor", "0", "--format", "json")
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(json.loads(result.stdout)["malformed"], 1)
            result = self.run_script("--reports", str(reports), "--mode", "enforce", "--floor", "0")
            self.assertIn("were not hits** and were skipped; a partial line fails CI Gate.", result.stdout)


if __name__ == "__main__":
    unittest.main()
