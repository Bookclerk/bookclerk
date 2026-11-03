#!/usr/bin/env python3
"""contract_coverage.py: declared pairs, hit parsing, the report and its rendering.

Everything runs offline against small documents and report texts; `MainTest`
runs the whole script against a temporary reports directory and the
repository's committed document.
"""

from __future__ import annotations

import importlib.util
import json
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
        self.assertIn("are reported only", text)
        self.assertIn("| `invalid` | `GET` | `/api/things/{id}` | 200 | `application/json` | a \\| b |", text)
        self.assertIn("| Things | 3 | 0 | 0.0% |", text)
        self.assertIn("fail CI Gate", cc.render(report, "enforce"))
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


class MainTest(unittest.TestCase):
    def run_script(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-I", str(ROOT / "scripts" / "contract_coverage.py"), *args],
            capture_output=True,
            text=True,
            cwd=ROOT,
            check=False,
        )

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


if __name__ == "__main__":
    unittest.main()
