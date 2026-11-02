#!/usr/bin/env python3
"""Tests for scripts/mutation_report.py: list parsing, summary and workflow wiring.

The lists are written by cargo-mutants in the same job, so they are not an
untrusted-input boundary; the parser still keeps every line printable and
bounded, and a seeded fuzz test checks that against an independent reading.
"""

from __future__ import annotations

import importlib.util
import os
import random
import re
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
        text = mr.render(out, mr.Run.CLEAN, stopped=False, widened=["crates/a/src/lib.rs", "crates/a/tests/t.rs"])
        self.assertIn("every function in them was mutated", text)
        self.assertIn("- `crates/a/src/lib.rs`", text)
        # A widened file that yielded no mutant (a test file) is not named.
        self.assertNotIn("tests/t.rs", text)
        self.assertNotIn("every function", mr.render(out, mr.Run.CLEAN, stopped=False))


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
        self.assertIn("No mutants to test", mr.render(mr.Results(), mr.Run.CLEAN, stopped=False))
        only_unviable = mr.render(results(unviable=2), mr.Run.CLEAN, stopped=False)
        self.assertIn("| 0 | 0 | 0 | 2 |", only_unviable)
        self.assertNotIn("%**", only_unviable)

    def test_an_error_after_partial_results_is_flagged(self) -> None:
        partial = mr.render(results(caught=1), mr.Run.ERROR, stopped=False)
        self.assertIn("**100%**", partial)
        self.assertIn("results are incomplete", partial)
        # The time-limit note already explains a stopped run.
        stopped = mr.render(results(caught=1), mr.Run.ERROR, stopped=True)
        self.assertNotIn("results are incomplete", stopped)
        self.assertNotIn("results are incomplete", mr.render(results(caught=1), mr.Run.CLEAN, stopped=False))

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


def names_of(job: dict) -> list[str | None]:
    """The job's step names, in order."""
    return [s.get("name") for s in job["steps"]]


class WidenDiffTest(unittest.TestCase):
    """The workflow's git commands put a widened file in the diff whole."""

    def test_git_rebuilds_the_diff(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "mutation.yml").read_text(encoding="utf-8")
        script = re.search(r"- name: Widen to files changed outside functions\n\s+run: \|\n((?:\s{10}.*\n)+)", workflow)
        assert script
        body = "\n".join(line[10:] for line in script.group(1).splitlines())
        # Everything after the cargo-mutants listing, which is stubbed.
        body = body.replace('cargo mutants --workspace --in-diff "$RUNNER_TEMP/pr.diff" --list', 'cat "$RUNNER_TEMP/stub-list.txt"')
        with tempfile.TemporaryDirectory() as tmp:
            repo, temp = Path(tmp) / "repo", Path(tmp) / "temp"
            repo.mkdir()
            temp.mkdir()
            git = ["git", "-c", "user.email=t@t", "-c", "user.name=t", "-C", str(repo)]
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            (repo / "data.rs").write_text("fn a() -> u32 { X }\nconst X: u32 = 1;\n", encoding="utf-8")
            (repo / "code.rs").write_text("fn b() -> bool { true }\n", encoding="utf-8")
            subprocess.run([*git, "add", "."], check=True)
            subprocess.run([*git, "commit", "-qm", "base"], check=True)
            (repo / "data.rs").write_text("fn a() -> u32 { X }\nconst X: u32 = 2;\n", encoding="utf-8")
            (repo / "code.rs").write_text("fn b() -> bool { false }\n", encoding="utf-8")
            subprocess.run([*git, "commit", "-qam", "change"], check=True)
            diff = subprocess.run([*git, "diff", "HEAD^1", "HEAD"], check=True, capture_output=True, text=True).stdout
            (temp / "pr.diff").write_text(diff, encoding="utf-8")
            (temp / "stub-list.txt").write_text("code.rs:1:18: replace b -> bool with true\n", encoding="utf-8")
            env = {"RUNNER_TEMP": str(temp), "PATH": os.environ["PATH"]}
            body = body.replace("scripts/mutation_report.py", str(ROOT / "scripts" / "mutation_report.py"))
            subprocess.run(["bash", "-c", body], cwd=repo, env=env, check=True)
            scoped = (temp / "pr.diff").read_text(encoding="utf-8")
        self.assertEqual(scoped.count("+++ b/"), 2, scoped)
        self.assertIn("--- /dev/null\n+++ b/data.rs\n@@ -0,0 +1,2 @@\n+fn a() -> u32 { X }\n+const X: u32 = 2;\n", scoped)
        self.assertIn("-fn b() -> bool { true }\n+fn b() -> bool { false }\n", scoped)


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
        self.assertEqual(list(triggers), ["pull_request"])
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        job = workflow["jobs"]["rust"]
        self.assertEqual(job["permissions"], {"contents": "read"})
        steps = {s.get("name"): s for s in job["steps"]}
        self.assertFalse(job["steps"][0]["with"]["persist-credentials"])
        # The merge commit (the default ref), so the workflow's scripts are
        # there even on a branch cut before they were added; its diff against
        # the base is what gets mutated.
        self.assertNotIn("ref", job["steps"][0]["with"])
        self.assertEqual(job["steps"][0]["with"]["fetch-depth"], 2)
        self.assertEqual(steps["Diff the pull request"]["run"], 'git diff HEAD^1 HEAD > "$RUNNER_TEMP/pr.diff"')
        run = steps["Test the mutants"]["run"]
        self.assertIn('--in-diff "$RUNNER_TEMP/pr.diff"', run)
        # Every workspace member, not only the default members.
        self.assertIn("--workspace", run)
        self.assertEqual(triggers["pull_request"]["paths"][0], "**/*.rs")
        self.assertIn("timeout --signal=INT", run)
        # rust-cache exports CARGO_INCREMENTAL=0; only a step value wins.
        self.assertEqual(steps["Test the mutants"]["env"]["CARGO_INCREMENTAL"], "1")
        self.assertIn("python3 scripts/mutation_report.py", steps["Report"]["run"])
        self.assertIn('--widened "$RUNNER_TEMP/widened.txt"', steps["Report"]["run"])
        widen = steps["Widen to files changed outside functions"]["run"]
        self.assertIn("--list", widen)
        self.assertIn("mutation_report.py widen", widen)
        self.assertLess(names_of(job).index("Widen to files changed outside functions"), names_of(job).index("Test the mutants"))
        prereqs = steps["Build the test prerequisites"]["run"]
        self.assertIn("ci-exec.py resolve --force-full", prereqs)
        self.assertIn("ci-exec.py prereqs rust_test --github-env", prereqs)
        names = [s.get("name") for s in job["steps"]]
        self.assertLess(names.index("Build the test prerequisites"), names.index("Test the mutants"))
        # The run's time limit leaves room to report inside the job's.
        self.assertIn("50m", run)
        self.assertGreaterEqual(job["timeout-minutes"], 70)


if __name__ == "__main__":
    unittest.main()
