#!/usr/bin/env python3
"""Tests for scripts/mutation_report.py: list parsing, summary and workflow wiring.

The lists are written by cargo-mutants in the same job, so they are not an
untrusted-input boundary; the parser still keeps every line printable and
bounded, and a seeded fuzz test checks that against an independent reading.
"""

from __future__ import annotations

import importlib.util
import json
import os
import random
import subprocess
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


DIFF = """\
diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs
--- a/crates/a/src/lib.rs
+++ b/crates/a/src/lib.rs
@@ -1 +1 @@
-const X: u32 = 1;
+const X: u32 = 2;
diff --git a/crates/b/src/api.rs b/crates/b/src/api.rs
--- a/crates/b/src/api.rs
+++ b/crates/b/src/api.rs
@@ -3 +3 @@
-    a < b
+    a <= b
diff --git a/crates/c/src/gone.rs b/crates/c/src/gone.rs
--- a/crates/c/src/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-fn f() {}
diff --git a/docs/x.md b/docs/x.md
--- a/docs/x.md
+++ b/docs/x.md
@@ -1 +1 @@
-a
+b
"""


class WidenTest(unittest.TestCase):
    """Changed Rust files the diff gave no mutant are mutated whole."""

    def test_changed_rust_files(self) -> None:
        self.assertEqual(mr.changed_rust_files(DIFF), ["crates/a/src/lib.rs", "crates/b/src/api.rs"])
        self.assertEqual(mr.changed_rust_files(DIFF + DIFF), ["crates/a/src/lib.rs", "crates/b/src/api.rs"])

    def test_files_without_a_mutant_are_widened(self) -> None:
        listed = "crates/b/src/api.rs:3:7: replace <= with > in f\n"
        self.assertEqual(mr.files_to_widen(DIFF, listed), ["crates/a/src/lib.rs"])
        self.assertEqual(mr.files_to_widen(DIFF, ""), ["crates/a/src/lib.rs", "crates/b/src/api.rs"])
        both = listed + "crates/a/src/lib.rs:9:1: replace g -> u32 with 0\n"
        self.assertEqual(mr.files_to_widen(DIFF, both), [])

    def test_widen_command(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            diff, listed = Path(tmp) / "pr.diff", Path(tmp) / "listed.txt"
            diff.write_text(DIFF, encoding="utf-8")
            listed.write_text("crates/b/src/api.rs:3:7: replace <= with > in f\n", encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["widen", "--diff", str(diff), "--listed", str(listed)]), 0)
        self.assertEqual("".join(c.args[0] for c in stdout.write.call_args_list), "crates/a/src/lib.rs\n")

    def test_widened_files_are_named_when_mutated(self) -> None:
        out = mr.Results()
        out.mutants[Outcome.CAUGHT] = ["crates/a/src/lib.rs:9:1: replace g -> u32 with 0"]
        text = mr.render(out, mr.Run.CLEAN, stopped=None, widened=["crates/a/src/lib.rs", "crates/a/tests/t.rs"])
        self.assertIn("the functions in them were mutated instead", text)
        self.assertNotIn("mutants, more than one run tests", text)
        self.assertIn("- `crates/a/src/lib.rs`", text)
        # A widened file that yielded no mutant (a test file) is not named.
        self.assertNotIn("tests/t.rs", text)
        self.assertNotIn("functions in them", mr.render(out, mr.Run.CLEAN, stopped=None))
        # The note counts the widened mutants that ran, not those admitted
        # (an unviable one, or one a stopped run never reached, did not run).
        out.mutants[Outcome.UNVIABLE] = ["crates/a/src/lib.rs:12:1: replace h with ()"]
        out.mutants[Outcome.MISSED] = ["crates/b/src/api.rs:3:7: replace <= with > in f"]
        sampled = mr.render(out, mr.Run.CLEAN, stopped=None, widened=["crates/a/src/lib.rs"], sample=mr.Sample(300, 15))
        self.assertIn("including any not listed, hold 300 mutants", sampled)
        self.assertIn("so 1 spread across them were tested", sampled)
        everything = mr.render(out, mr.Run.CLEAN, stopped=None, widened=["crates/a/src/lib.rs"], sample=mr.Sample(9, 9))
        self.assertNotIn("hold 9 mutants", everything)


class IssueTest(unittest.TestCase):
    """The issue body is written only when a mutant was missed."""

    def test_write_issue(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            body = Path(tmp, "issue.md")
            for found in (None, results(caught=2), results(timeout=1)):
                mr.write_issue(body, "summary", found)
                self.assertFalse(body.exists())
            mr.write_issue(None, "summary", results(missed=1))
            mr.write_issue(body, "summary", results(caught=1, missed=1))
            self.assertEqual(body.read_text(encoding="utf-8"), "summary")


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
        text = mr.render(out, mr.Run.MISSED, stopped=None)
        self.assertIn("**75%** of the 4 mutants", text)
        self.assertIn("| 3 | 1 | 0 | 0 |", text)
        self.assertIn(f"- `{MISSED}`", text)
        self.assertIn("Daily run", text)
        self.assertNotIn("time limit", text)

    def test_long_lists_are_cut(self) -> None:
        text = mr.render(results(missed=mr.MAX_LISTED + 7), mr.Run.MISSED, stopped=mr.Stop.TIME_LIMIT)
        self.assertEqual(text.count("\n- `"), mr.MAX_LISTED)
        self.assertIn("and 7 more", text)
        self.assertIn("time limit", text)

    def test_other_states(self) -> None:
        self.assertIn("failed to build or its tests failed", mr.render(mr.Results(), mr.Run.BASELINE_FAILED, stopped=None))
        self.assertIn("did not finish", mr.render(mr.Results(), mr.Run.ERROR, stopped=None))
        self.assertIn("No mutants to test", mr.render(mr.Results(), mr.Run.CLEAN, stopped=None))
        only_unviable = mr.render(results(unviable=2), mr.Run.CLEAN, stopped=None)
        self.assertIn("| 0 | 0 | 0 | 2 |", only_unviable)
        self.assertNotIn("%**", only_unviable)

    def test_an_error_after_partial_results_is_flagged(self) -> None:
        partial = mr.render(results(caught=1), mr.Run.ERROR, stopped=None)
        self.assertIn("**100%**", partial)
        self.assertIn("results are incomplete", partial)
        # The time-limit note already explains a stopped run.
        stopped = mr.render(results(caught=1), mr.Run.ERROR, stopped=mr.Stop.TIME_LIMIT)
        self.assertNotIn("results are incomplete", stopped)
        self.assertNotIn("results are incomplete", mr.render(results(caught=1), mr.Run.CLEAN, stopped=None))

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
            # The range was not tested: the run fails so the next one retries it.
            self.assertEqual(mr.main(["--out", str(Path(tmp) / "none"), "--exit-code", "1", "--summary", ""]), 1)
        self.assertIn("did not finish", "".join(c.args[0] for c in stdout.write.call_args_list))

    def test_usable(self) -> None:
        self.assertTrue(mr.usable(mr.Run.CLEAN, None))
        self.assertTrue(mr.usable(mr.Run.MISSED, None))
        self.assertTrue(mr.usable(mr.Run.TIMEOUTS, None))
        self.assertFalse(mr.usable(mr.Run.BASELINE_FAILED, None))
        self.assertFalse(mr.usable(mr.Run.ERROR, None))
        # Stopped at the time limit, or before the runner ran out of disk or
        # memory (SIGINT reads as an error): what it tested stands.
        for stop in mr.Stop:
            self.assertTrue(mr.usable(mr.Run.ERROR, stop))

    def test_stop_notes_name_the_reason(self) -> None:
        self.assertEqual(set(mr.STOP_NOTES), set(mr.Stop))
        for stop, needle in ((mr.Stop.TIME_LIMIT, "time limit"), (mr.Stop.LOW_DISK, "disk space"), (mr.Stop.LOW_MEMORY, "memory")):
            text = mr.render(results(caught=1), mr.Run.ERROR, stopped=stop)
            self.assertIn(f"> {mr.STOP_NOTES[stop]}", text)
            self.assertIn(needle, text)
            self.assertNotIn("results are incomplete", text)

    def test_stopped_flag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "mutants.out"
            out.mkdir()
            (out / "caught.txt").write_text(CAUGHT + "\n", encoding="utf-8")
            cases = (([], 1, None), (["--stopped"], 0, mr.Stop.TIME_LIMIT), (["--stopped", "low-disk"], 0, mr.Stop.LOW_DISK), (["--stopped", "low-memory"], 0, mr.Stop.LOW_MEMORY))
            for extra, code, stop in cases:
                with self.subTest(extra=extra), mock.patch("sys.stdout") as stdout:
                    self.assertEqual(mr.main(["--out", str(out), "--exit-code", "130", "--summary", "", *extra]), code)
                    text = "".join(c.args[0] for c in stdout.write.call_args_list)
                    for other in mr.Stop:
                        (self.assertIn if other is stop else self.assertNotIn)(mr.STOP_NOTES[other], text)
            with mock.patch("sys.stderr"), self.assertRaises(SystemExit):
                mr.main(["--out", str(out), "--exit-code", "130", "--stopped", "tired"])


def names_of(job: dict) -> list[str | None]:
    """The job's step names, in order."""
    return [s.get("name") for s in job["steps"]]


def listing(*spans: tuple[str, int, int]) -> str:
    """A ``cargo mutants --list --json`` listing of mutants with these spans."""
    return json.dumps(
        [{"file": path, "name": f"{path}:{start}:1: m", "span": {"start": {"line": start, "column": 1}, "end": {"line": end, "column": 9}}} for path, start, end in spans]
    )


class SampleTest(unittest.TestCase):
    """Widened files' mutants are marked line by line, within the budget."""

    LISTED = listing(*(("crates/a/src/lib.rs", line, line) for line in range(1, 11)))

    def test_within_budget_every_line_is_marked(self) -> None:
        lines, sample = mr.sample_lines(self.LISTED, budget=10, seed=7)
        self.assertEqual(lines, {"crates/a/src/lib.rs": list(range(1, 11))})
        self.assertEqual(sample, mr.Sample(10, 10))

    def test_past_the_budget_a_rotating_spread_is_marked(self) -> None:
        picks = []
        for seed in range(3):
            lines, sample = mr.sample_lines(self.LISTED, budget=4, seed=seed)
            self.assertEqual(sample, mr.Sample(10, 4))
            picks.append(lines["crates/a/src/lib.rs"])
        self.assertEqual(picks[0], [1, 4, 7, 10])
        self.assertNotEqual(picks[0], picks[1])

    def test_a_small_file_is_not_crowded_out(self) -> None:
        """Each widened file gets a line, however many mutants the others hold."""
        spans = [("big.rs", line, line) for line in range(1, 101)] + [("small.rs", 5, 5)]
        for seed in range(150):
            lines, sample = mr.sample_lines(listing(*spans), budget=15, seed=seed)
            self.assertEqual(lines.get("small.rs"), [5], seed)
            self.assertEqual(sample, mr.Sample(101, 15))

    def test_shared_spans_count_against_the_budget(self) -> None:
        """Regression: body replacements span their function, so one line admits all of them."""
        functions = [("f.rs", 10 * i + 1, 10 * i + 9) for i in range(15)]
        spans = [s for f in functions for s in (f, f)] + [("f.rs", start + 3, start + 3) for _, start, _ in functions]
        for budget in (1, 5, 15, 29):
            for seed in range(4):
                lines, sample = mr.sample_lines(listing(*spans), budget=budget, seed=seed)
                admitted = sum(1 for path, start, end in spans if any(start <= line <= end for line in lines.get(path, [])))
                self.assertEqual(sample.tested, admitted)
                self.assertEqual(sample.total, 45)
                # A function's line admits at least its two body replacements
                # and the operator on it; below that, the smallest line still runs.
                self.assertLessEqual(admitted, max(budget, 3))
                self.assertGreater(admitted, 0)

    def test_fuzz_admitted_matches_an_independent_count(self) -> None:
        rng = random.Random(575)
        for _ in range(300):
            spans = []
            for _ in range(rng.randrange(0, 30)):
                start = rng.randrange(1, 40)
                spans.append((rng.choice(["a.rs", "b.rs"]), start, start + rng.choice([0, 0, 1, 5, 20])))
            budget, seed = rng.randrange(0, 35), rng.randrange(0, 1000)
            lines, sample = mr.sample_lines(listing(*spans), budget=budget, seed=seed)
            admitted = sum(1 for path, start, end in spans if any(start <= line <= end for line in lines.get(path, [])))
            self.assertEqual(sample, mr.Sample(len(spans), admitted))
            smallest = min((sum(1 for p, s, e in spans if p == path and s <= start <= e) for path, start, _ in spans), default=0)
            self.assertLessEqual(admitted, max(budget, 1, smallest))
            self.assertEqual(admitted > 0, bool(spans))
            if len(spans) <= max(budget, 1):
                self.assertEqual(admitted, len(spans))

    def test_malformed_listings(self) -> None:
        for bad in ("", "{", "{}", "[1]", '[{"file": "a.rs"}]', '[{"file": "a.rs", "span": {"start": {"line": 0}, "end": {"line": 1}}}]',
                    '[{"file": "a.rs", "span": {"start": {"line": 3}, "end": {"line": 2}}}]', '[{"file": 1, "span": {"start": {"line": 1}, "end": {"line": 1}}}]'):
            self.assertEqual(mr.parse_spans(bad), [], bad)
            self.assertEqual(mr.sample_lines(bad, 5, 0), ({}, mr.Sample(0, 0)))

    def test_drop_files(self) -> None:
        self.assertEqual(mr.changed_rust_files(mr.drop_files(DIFF, {"crates/a/src/lib.rs"})), ["crates/b/src/api.rs"])
        self.assertIn("docs/x.md", mr.drop_files(DIFF, {"crates/a/src/lib.rs"}))
        self.assertEqual(mr.drop_files(DIFF, set()), DIFF)

    def test_line_hunks(self) -> None:
        source = "a\nb\r\nc\nd\n"
        self.assertEqual(
            mr.line_hunks("x.rs", source, [1, 2, 4, 9]),
            "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -0,0 +1,2 @@\n+a\n+b\n@@ -3,0 +4,1 @@\n+d\n",
        )
        self.assertEqual(mr.line_hunks("x.rs", "a", [1]).splitlines()[-1], "+a")
        self.assertEqual(mr.line_hunks("x.rs", source, [9]), "")

    def test_line_hunks_apply_with_git(self) -> None:
        """Oracle: git applies each section to an empty file of as many lines."""
        rng = random.Random(573)
        for _ in range(40):
            count = rng.randrange(1, 12)
            text = [f"line {i} {rng.choice(['', 'x', '{ }'])}" for i in range(1, count + 1)]
            picked = sorted(rng.sample(range(1, count + 1), rng.randrange(1, count + 1)))
            section = mr.line_hunks("f.rs", "\n".join(text) + "\n", picked)
            with tempfile.TemporaryDirectory() as tmp:
                # The old side: the file without the picked lines.
                Path(tmp, "f.rs").write_text("".join(t + "\n" for i, t in enumerate(text, 1) if i not in picked), encoding="utf-8")
                subprocess.run(["git", "init", "-q", tmp], check=True)
                Path(tmp, "p.diff").write_text(section, encoding="utf-8")
                subprocess.run(["git", "-C", tmp, "apply", "--unidiff-zero", "--whitespace=nowarn", "p.diff"], check=True)
                self.assertEqual(Path(tmp, "f.rs").read_text(encoding="utf-8"), "\n".join(text) + "\n")

    def test_sample_command(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            cwd = os.getcwd()
            os.chdir(tmp)
            try:
                Path("crates/a/src").mkdir(parents=True)
                Path("crates/a/src/lib.rs").write_text("".join(f"fn f{i}() {{}}\n" for i in range(1, 11)), encoding="utf-8")
                Path("pr.diff").write_text(DIFF, encoding="utf-8")
                Path("listed.txt").write_text(self.LISTED, encoding="utf-8")
                argv = ["sample", "--diff", "pr.diff", "--listed", "listed.txt", "--budget", "5", "--seed", "1", "--record", "s.json"]
                self.assertEqual(mr.main(argv), 0)
                diff = Path("pr.diff").read_text(encoding="utf-8")
                record = mr.read_sample(Path("s.json").read_text(encoding="utf-8"))
            finally:
                os.chdir(cwd)
        self.assertEqual(record, mr.Sample(10, 5))
        # The original section of the widened file is replaced by its marked lines.
        self.assertNotIn("const X", diff)
        self.assertEqual(diff.count("+fn f"), 5)
        self.assertIn("crates/b/src/api.rs", diff)

    def test_read_sample(self) -> None:
        self.assertEqual(mr.read_sample('{"total": 3, "tested": 1}'), mr.Sample(3, 1))
        for bad in ("", "[]", "{}", '{"total": -1, "tested": 0}', '{"total": 3, "tested": 4}', '{"total": "x", "tested": 1}'):
            self.assertIsNone(mr.read_sample(bad), bad)


class WiringTest(unittest.TestCase):
    """The workflow and cargo-mutants settings stay consistent with the repository."""

    def test_generated_files_are_excluded(self) -> None:
        mutants = tomllib.loads((ROOT / ".cargo" / "mutants.toml").read_text(encoding="utf-8"))
        areas = tomllib.loads((ROOT / "scripts" / "coverage_areas.toml").read_text(encoding="utf-8"))
        generated = sorted(p for p in areas["generated"] if p.endswith(".rs"))
        self.assertEqual(sorted(g for g in mutants["exclude_globs"] if not g.endswith("/**")), generated)
        # The e2e harness needs staged helpers this workflow does not build.
        self.assertIn("crates/bookclerk-plugin-e2e/**", mutants["exclude_globs"])
        self.assertEqual(mutants["test_tool"], "nextest")

    def test_settings_match_ci(self) -> None:
        import yaml

        sys.path.insert(0, str(ROOT / "scripts"))
        from ci_plan import execute

        mutants = tomllib.loads((ROOT / ".cargo" / "mutants.toml").read_text(encoding="utf-8"))
        # The custom-harness binaries CI runs with `cargo test` stay out of nextest.
        self.assertEqual(mutants["additional_cargo_test_args"], execute._nextest_filter(execute.CUSTOM_HARNESS_TESTS))
        # Lints are capped for the whole job, so prerequisites and mutants share flags.
        workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "mutation.yml").read_text(encoding="utf-8"))
        self.assertFalse(mutants["cap_lints"])
        self.assertEqual(workflow["env"]["RUSTFLAGS"], "--cap-lints=warn")
        # The same enforcement switches as CI's test job.
        ci = yaml.safe_load((ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8"))
        self.assertEqual(workflow["jobs"]["rust"]["env"], ci["jobs"]["test"]["env"])

    def test_workflow(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "mutation.yml").read_text(encoding="utf-8"))
        triggers = workflow.get("on", workflow.get(True))
        self.assertEqual(sorted(triggers), ["schedule", "workflow_dispatch"])
        self.assertIn("base", triggers["workflow_dispatch"]["inputs"])
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        job = workflow["jobs"]["rust"]
        self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
        steps = {s.get("name"): s for s in job["steps"]}
        self.assertFalse(job["steps"][0]["with"]["persist-credentials"])
        # The default branch with full history; its diff against the last
        # successful run's commit is what gets mutated.
        self.assertNotIn("ref", job["steps"][0]["with"])
        self.assertEqual(job["steps"][0]["with"]["fetch-depth"], 0)
        diff = steps["Diff the code merged since the last run"]["run"]
        self.assertIn("scripts/mutation_base.py --workflow mutation.yml", diff)
        self.assertIn('git diff "$base" HEAD > "$RUNNER_TEMP/pr.diff"', diff)
        run = steps["Test the mutants"]["run"]
        self.assertIn('--in-diff "$RUNNER_TEMP/pr.diff"', run)
        # Every workspace member, not only the default members.
        self.assertIn("--workspace", run)
        self.assertIn("timeout --signal=INT", run)
        # rust-cache exports CARGO_INCREMENTAL=0; only a step value wins.
        self.assertEqual(steps["Test the mutants"]["env"]["CARGO_INCREMENTAL"], "1")
        self.assertIn("python3 scripts/mutation_report.py", steps["Report"]["run"])
        self.assertIn('--widened "$RUNNER_TEMP/widened.txt"', steps["Report"]["run"])
        self.assertIn('--sample "$RUNNER_TEMP/sample.json"', steps["Report"]["run"])
        widen_step = steps["Widen to files changed outside functions"]
        self.assertEqual(widen_step["env"]["SEED"], "${{ github.run_number }}")
        self.assertGreater(int(widen_step["env"]["WIDEN_BUDGET"]), 0)
        widen = steps["Widen to files changed outside functions"]["run"]
        self.assertIn("--list", widen)
        self.assertIn("mutation_report.py widen", widen)
        self.assertIn("mutation_report.py sample", widen)
        self.assertLess(names_of(job).index("Widen to files changed outside functions"), names_of(job).index("Test the mutants"))
        prereqs = steps["Build the test prerequisites"]["run"]
        self.assertIn("ci-exec.py resolve --force-full", prereqs)
        self.assertIn("ci-exec.py prereqs rust_test --github-env", prereqs)
        names = [s.get("name") for s in job["steps"]]
        self.assertLess(names.index("Build the test prerequisites"), names.index("Test the mutants"))
        # The run's time limit leaves room to report inside the job's.
        self.assertIn("240m", run)
        self.assertGreaterEqual(job["timeout-minutes"], 250)
        self.assertIn('--issue "$RUNNER_TEMP/issue/rust.md"', steps["Report"]["run"])
        assert_files_issues(self, workflow, "rust", "rust")

    def test_issue_workflow(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "mutation-issue.yml").read_text(encoding="utf-8"))
        triggers = workflow.get("on", workflow.get(True))
        self.assertEqual(list(triggers), ["workflow_call"])
        self.assertEqual(workflow["permissions"], {})
        job = workflow["jobs"]["issue"]
        # The only write token: no checkout, so no repository code runs with it.
        self.assertEqual(job["permissions"], {"issues": "write"})
        # Branch runs never write to the shared issues.
        self.assertEqual(job["if"], "${{ github.ref == 'refs/heads/main' }}")
        self.assertIn("--limit 1000", job["steps"][1]["run"])
        self.assertFalse(any("actions/checkout" in s.get("uses", "") for s in job["steps"]))
        self.assertEqual(job["steps"][0]["with"]["pattern"], "mutation-issue-*")
        run = job["steps"][1]["run"]
        self.assertIn("^[a-z][a-z0-9-]{0,30}$", run)
        self.assertIn("--body-file", run)
        self.assertNotIn("--body ", run)


def assert_files_issues(case: unittest.TestCase, workflow: dict, job: str, area: str) -> None:
    """Check that a mutation workflow uploads its issue body and files it.

    Args:
        case: The test asserting.
        workflow: The parsed workflow.
        job: The mutants job's id.
        area: The issue's area (``rust``, a Worker's name or ``python-sdk``);
            a matrix job passes its ``${{ matrix.name }}`` expression.
    """
    steps = {s.get("name"): s for s in workflow["jobs"][job]["steps"]}
    upload = steps["Upload the issue body"]["with"]
    case.assertEqual(upload["name"], f"mutation-issue-{area}")
    case.assertEqual(upload["if-no-files-found"], "ignore")
    issue = workflow["jobs"]["issue"]
    case.assertEqual(issue["needs"], job)
    case.assertEqual(issue["uses"], "./.github/workflows/mutation-issue.yml")
    case.assertEqual(issue["permissions"], {"issues": "write"})


if __name__ == "__main__":
    unittest.main()
