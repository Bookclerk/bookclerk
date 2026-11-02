#!/usr/bin/env python3
"""Tests for scripts/mutation_report.py: list parsing, summary and workflow wiring.

The lists are written by cargo-mutants in the same job, so they are not an
untrusted-input boundary; the parser still keeps every line printable and
bounded, and a seeded fuzz test checks that against an independent reading.
"""

from __future__ import annotations

import importlib.util
import random
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("mutation_report", ROOT / "scripts" / "mutation_report.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutation_report"] = module
    spec.loader.exec_module(module)
    return module


mr = _load()
Outcome = mr.Outcome

MISSED = "crates/a/src/lib.rs:12:5: replace is_ready -> bool with true"
CAUGHT = "crates/a/src/lib.rs:20:9: replace < with <= in limit"


def results(**counts: int) -> object:
    """Results with ``counts[outcome]`` numbered mutants per outcome."""
    out = mr.Results()
    for name, n in counts.items():
        out.mutants[Outcome[name.upper()]] = [f"crates/a/src/lib.rs:{i}:1: replace f{i}" for i in range(n)]
    return out


class ParseTest(unittest.TestCase):
    """``parse_list`` reads one mutant per line."""

    def test_reads_lines(self) -> None:
        self.assertEqual(mr.parse_list(f"{MISSED}\n\n  {CAUGHT}  \n"), [MISSED, CAUGHT])

    def test_cleans_lines(self) -> None:
        self.assertEqual(mr.parse_list("a\x1b[31mb\tc\r\n"), ["a?[31mb?c"])
        self.assertEqual(len(mr.parse_list("x" * 1000)[0]), mr.MAX_LINE)

    def test_fuzz_matches_an_independent_reading(self) -> None:
        rng = random.Random(568)
        alphabet = ["a", "`", " ", "\n", "\r", "\t", "\x00", "é", " ", ":", "|", "*"]
        for _ in range(500):
            text = "".join(rng.choice(alphabet) for _ in range(rng.randrange(0, 80)))
            parsed = mr.parse_list(text)
            for line in parsed:
                self.assertTrue(line and line.isprintable() and len(line) <= mr.MAX_LINE, repr(line))
            # Oracle: the non-blank lines, in order, each the same length.
            expected = [ln.strip() for ln in text.splitlines() if ln.strip()]
            self.assertEqual([len(p) for p in parsed], [min(len(e), mr.MAX_LINE) for e in expected])


class ScoreTest(unittest.TestCase):
    """The score counts timeouts as caught and leaves out unviable mutants."""

    def test_score(self) -> None:
        self.assertEqual(results(caught=6, missed=2, timeout=2, unviable=9).score(), 80.0)
        self.assertIsNone(results(unviable=3).score())

    def test_exit_codes(self) -> None:
        self.assertIs(mr.run_state(0), mr.Run.CLEAN)
        self.assertIs(mr.run_state(2), mr.Run.MISSED)
        self.assertIs(mr.run_state(3), mr.Run.TIMEOUTS)
        self.assertIs(mr.run_state(4), mr.Run.BASELINE_FAILED)
        for code in (1, 124, 130, -1):
            self.assertIs(mr.run_state(code), mr.Run.ERROR)


class RenderTest(unittest.TestCase):
    """The summary reports the score, the counts and the missed mutants."""

    def test_missed_mutants_are_listed(self) -> None:
        out = mr.Results()
        out.mutants[Outcome.CAUGHT] = [CAUGHT] * 3
        out.mutants[Outcome.MISSED] = [MISSED]
        text = mr.render(out, mr.Run.MISSED, stopped=False)
        self.assertIn("**75%** of the 4 mutants", text)
        self.assertIn("| 3 | 1 | 0 | 0 |", text)
        self.assertIn(f"- `{MISSED}`", text)
        self.assertIn("Report only", text)
        self.assertNotIn("time limit", text)

    def test_long_lists_are_cut(self) -> None:
        text = mr.render(results(missed=mr.MAX_LISTED + 7), mr.Run.MISSED, stopped=True)
        self.assertEqual(text.count("\n- `"), mr.MAX_LISTED)
        self.assertIn("and 7 more", text)
        self.assertIn("time limit", text)

    def test_other_states(self) -> None:
        self.assertIn("failed to build or its tests failed", mr.render(mr.Results(), mr.Run.BASELINE_FAILED, stopped=False))
        self.assertIn("did not finish", mr.render(mr.Results(), mr.Run.ERROR, stopped=False))
        self.assertIn("touches no Rust code", mr.render(mr.Results(), mr.Run.CLEAN, stopped=False))
        only_unviable = mr.render(results(unviable=2), mr.Run.CLEAN, stopped=False)
        self.assertIn("| 0 | 0 | 0 | 2 |", only_unviable)
        self.assertNotIn("%**", only_unviable)

    def test_backticks_stay_in_code(self) -> None:
        self.assertEqual(mr._code("a"), "`a`")
        self.assertEqual(mr._code("a `b` c"), "`` a `b` c ``")
        self.assertEqual(mr._code("x``y"), "``` x``y ```")


class MainTest(unittest.TestCase):
    """The script reads a ``mutants.out`` directory and appends to the summary file."""

    def test_end_to_end(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "mutants.out"
            out.mkdir()
            (out / "missed.txt").write_text(MISSED + "\n", encoding="utf-8")
            (out / "caught.txt").write_text(CAUGHT + "\n", encoding="utf-8")
            summary = Path(tmp) / "summary.md"
            summary.write_text("before\n", encoding="utf-8")
            with mock.patch("sys.stdout"):
                self.assertEqual(mr.main(["--out", str(out), "--exit-code", "2", "--summary", str(summary)]), 0)
            text = summary.read_text(encoding="utf-8")
            self.assertTrue(text.startswith("before\n## Mutation testing (Rust)"))
            self.assertIn("**50%** of the 2 mutants", text)

    def test_missing_directory_reports_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch("sys.stdout") as stdout:
            self.assertEqual(mr.main(["--out", str(Path(tmp) / "none"), "--exit-code", "1", "--summary", ""]), 0)
        self.assertIn("did not finish", "".join(c.args[0] for c in stdout.write.call_args_list))


class WiringTest(unittest.TestCase):
    """The workflow and cargo-mutants settings stay consistent with the repository."""

    def test_generated_files_are_excluded(self) -> None:
        mutants = tomllib.loads((ROOT / ".cargo" / "mutants.toml").read_text(encoding="utf-8"))
        areas = tomllib.loads((ROOT / "scripts" / "coverage_areas.toml").read_text(encoding="utf-8"))
        self.assertEqual(sorted(mutants["exclude_globs"]), sorted(p for p in areas["generated"] if p.endswith(".rs")))
        self.assertEqual(mutants["test_tool"], "nextest")
        self.assertTrue(mutants["cap_lints"])

    def test_workflow(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "mutation.yml").read_text(encoding="utf-8"))
        triggers = workflow.get("on", workflow.get(True))
        self.assertEqual(list(triggers), ["pull_request"])
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        job = workflow["jobs"]["rust"]
        self.assertEqual(job["permissions"], {"contents": "read", "pull-requests": "read"})
        steps = {s.get("name"): s for s in job["steps"]}
        self.assertFalse(job["steps"][0]["with"]["persist-credentials"])
        self.assertEqual(job["steps"][0]["with"]["ref"], "${{ github.event.pull_request.head.sha }}")
        run = steps["Test the mutants"]["run"]
        self.assertIn('--in-diff "$RUNNER_TEMP/pr.diff"', run)
        self.assertIn("timeout --signal=INT", run)
        # rust-cache exports CARGO_INCREMENTAL=0; only a step value wins.
        self.assertEqual(steps["Test the mutants"]["env"]["CARGO_INCREMENTAL"], "1")
        self.assertIn("python3 scripts/mutation_report.py", steps["Report"]["run"])
        # The run's time limit leaves room to report inside the job's.
        self.assertIn("50m", run)
        self.assertGreaterEqual(job["timeout-minutes"], 70)


if __name__ == "__main__":
    unittest.main()
