#!/usr/bin/env python3
"""Tests for scripts/stryker_report.py: diff scoping, report parsing, summary and wiring.

The diff and the Stryker report are written in the same job, so they are not
an untrusted-input boundary; the parsers still keep every value bounded and
printable, and seeded fuzz tests check them against independent readings.
"""

from __future__ import annotations

import importlib.util
import json
import random
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    sys.path.insert(0, str(ROOT / "scripts"))
    spec = importlib.util.spec_from_file_location("stryker_report", ROOT / "scripts" / "stryker_report.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["stryker_report"] = module
    spec.loader.exec_module(module)
    return module


sr = _load()
Outcome = sr.Outcome

DIFF = """\
diff --git a/tools/site/src/page.ts b/tools/site/src/page.ts
--- a/tools/site/src/page.ts
+++ b/tools/site/src/page.ts
@@ -1,4 +1,5 @@
 const a = 1;
-const b = 2;
+const b = 3;
+const c = 4;
 const d = 5;
 const e = 6;
@@ -10,2 +11,3 @@ function f() {
 x();
+y();
 z();
diff --git a/tools/site/src/page.test.ts b/tools/site/src/page.test.ts
--- a/tools/site/src/page.test.ts
+++ b/tools/site/src/page.test.ts
@@ -1 +1 @@
-old
+new
diff --git a/tools/demo/src/a.ts b/tools/demo/src/a.ts
--- a/tools/demo/src/a.ts
+++ b/tools/demo/src/a.ts
@@ -1 +1 @@
-old
+new
diff --git a/tools/site/src/gone.ts b/tools/site/src/gone.ts
--- a/tools/site/src/gone.ts
+++ /dev/null
@@ -1 +0,0 @@
-gone
"""


def report(*mutants: dict[str, object], name: str = "src/page.ts") -> str:
    """A Stryker JSON report holding ``mutants`` in one file."""
    return json.dumps({"schemaVersion": "2", "files": {name: {"language": "typescript", "source": "", "mutants": list(mutants)}}})


def mutant(status: str, line: int = 3, mutator: str = "ConditionalExpression", replacement: str = "true") -> dict[str, object]:
    """One Stryker mutant entry."""
    return {
        "id": "1",
        "mutatorName": mutator,
        "replacement": replacement,
        "status": status,
        "location": {"start": {"line": line, "column": 5}, "end": {"line": line, "column": 9}},
    }


class LinesTest(unittest.TestCase):
    """The changed lines of a Worker's sources become Stryker's ``--mutate``."""

    def test_changed_lines(self) -> None:
        self.assertEqual(sr.changed_lines(DIFF, "tools/site"), {"src/page.ts": [(2, 3), (12, 12)]})
        self.assertEqual(sr.changed_lines(DIFF, "tools/demo/"), {"src/a.ts": [(1, 1)]})
        self.assertEqual(sr.changed_lines(DIFF, "tools/diagnostics-collector"), {})
        self.assertEqual(sr.mutate_patterns(sr.changed_lines(DIFF, "tools/site")), "src/page.ts:2-3,src/page.ts:12-12")
        self.assertEqual(sr.mutate_patterns({}), "")

    def test_sources(self) -> None:
        for path in ("worker.ts", "src/page.ts", "dashboard/db.ts", "build.ts"):
            self.assertTrue(sr.is_source(path), path)
        for path in ("a.test.ts", "a.fuzz.test.ts", "env.d.ts", "vitest.config.ts", "test/a.ts", "test-support/d1.ts",
                     "node_modules/x/a.ts", "coverage/a.ts", "a.js", "README.md", ""):
            self.assertFalse(sr.is_source(path), path)

    def test_unsafe_paths_are_left_out(self) -> None:
        odd = DIFF.replace("src/page.ts", "src/pa,ge.ts")
        self.assertNotIn("src/pa,ge.ts", sr.changed_lines(odd, "tools/site"))
        quoted = '+++ "b/tools/site/src/a\\nb.ts"\n@@ -0,0 +1 @@\n+x\n'
        self.assertEqual(sr.changed_lines(quoted, "tools/site"), {})
        climbing = "+++ b/tools/site/../x.ts\n@@ -0,0 +1 @@\n+x\n"
        self.assertEqual(sr.changed_lines(climbing, "tools/site"), {})
        self.assertEqual(sr.mutate_patterns({"a b.ts": [(1, 1)], "ok.ts": [(2, 4)]}), "ok.ts:2-4")

    def test_fuzz_matches_inserted_lines(self) -> None:
        """Oracle: unique lines inserted at known places are what git's diff adds."""
        rng = random.Random(576)
        for _ in range(40):
            before = [f"base {i}" for i in range(rng.randrange(0, 30))]
            after = list(before)
            for marker in range(rng.randrange(1, 8)):
                after.insert(rng.randrange(0, len(after) + 1), f"inserted {marker}")
            expected = sorted(i + 1 for i, text in enumerate(after) if text.startswith("inserted"))
            with tempfile.TemporaryDirectory() as tmp:
                old, new = Path(tmp, "old.ts"), Path(tmp, "new.ts")
                old.write_text("".join(t + "\n" for t in before), encoding="utf-8")
                new.write_text("".join(t + "\n" for t in after), encoding="utf-8")
                result = subprocess.run(
                    ["git", "diff", "--no-index", f"-U{rng.randrange(0, 4)}", "--", str(old), str(new)],
                    capture_output=True,
                    text=True,
                    check=False,
                )
            diff = result.stdout.replace(f"+++ b{new}", "+++ b/tools/site/w.ts").replace(f"+++ {new}", "+++ b/tools/site/w.ts")
            ranges = sr.changed_lines(diff, "tools/site").get("w.ts", [])
            self.assertEqual([line for start, end in ranges for line in range(start, end + 1)], expected)


class ReportTest(unittest.TestCase):
    """Stryker's statuses map onto the Rust job's outcomes."""

    def test_statuses(self) -> None:
        parsed = sr.parse_report(
            report(mutant("Killed"), mutant("Timeout"), mutant("Survived", 7), mutant("NoCoverage", 9), mutant("CompileError"), mutant("Ignored")),
            "tools/site",
        )
        assert parsed is not None
        results, survivors = parsed
        counts = {o: results.count(o) for o in Outcome}
        self.assertEqual(counts, {Outcome.CAUGHT: 1, Outcome.TIMEOUT: 1, Outcome.MISSED: 2, Outcome.UNVIABLE: 1})
        self.assertEqual([(p, line) for p, line, _ in survivors], [("tools/site/src/page.ts", 7), ("tools/site/src/page.ts", 9)])
        self.assertEqual(results.mutants[Outcome.MISSED][0], "tools/site/src/page.ts:7:5: ConditionalExpression → true")

    def test_malformed_reports(self) -> None:
        for bad in ("", "[]", "{}", '{"files": []}', "nope"):
            self.assertIsNone(sr.parse_report(bad, "tools/site"), bad)
        odd = json.dumps({"files": {"a.ts": {"mutants": [1, {"status": "Survived"}, {"status": "Killed", "location": {"start": {"line": -1}}}]}, "b.ts": 3}})
        parsed = sr.parse_report(odd, "tools/site")
        assert parsed is not None
        self.assertEqual(parsed[0].count(Outcome.MISSED), 1)
        self.assertEqual(parsed[1], [])  # no line, so no annotation
        self.assertEqual(parsed[0].mutants[Outcome.CAUGHT], ["tools/site/a.ts:0:0:  → (removed)"])

    def test_fuzz_fields_stay_printable_and_bounded(self) -> None:
        rng = random.Random(1576)
        values = [None, 1, -3, 10**9, "x", "a\nb\x1b[0m`", "é" * 400, [], {}, True]
        for _ in range(400):
            entry = {key: rng.choice(values) for key in ("status", "mutatorName", "replacement") if rng.random() < 0.9}
            if rng.random() < 0.7:
                entry["status"] = rng.choice(list(sr.STATUSES) + ["Ignored", "Weird"])
            if rng.random() < 0.7:
                entry["location"] = {"start": {"line": rng.choice(values), "column": rng.choice(values)}}
            parsed = sr.parse_report(json.dumps({"files": {rng.choice(["a.ts", "b\n.ts", "c`.ts"]): {"mutants": [entry]}}}), "tools/site")
            assert parsed is not None
            results, survivors = parsed
            expected = sr.STATUSES.get(entry.get("status")) if isinstance(entry.get("status"), str) else None
            self.assertEqual(sum(results.count(o) for o in Outcome), 0 if expected is None else 1)
            for outcome in Outcome:
                for description in results.mutants[outcome]:
                    self.assertTrue(description.isprintable() and len(description) <= sr.MAX_LINE, repr(description))
            for _, line, _ in survivors:
                self.assertGreater(line, 0)


class RenderTest(unittest.TestCase):
    """The summary and annotations."""

    def test_states(self) -> None:
        self.assertIn("touches no source line", sr.render("site", None, mutated=False))
        self.assertIn("did not finish", sr.render("site", None, mutated=True))
        empty = sr.parse_report(report(), "tools/site")
        self.assertIn("found no mutant", sr.render("site", empty, mutated=True))
        parsed = sr.parse_report(report(mutant("Killed"), mutant("Killed"), mutant("Killed"), mutant("Survived", 7)), "tools/site")
        text = sr.render("site", parsed, mutated=True)
        self.assertTrue(text.startswith("## Mutation testing (site Worker)\n"))
        self.assertIn("**75%** of the 4 mutants", text)
        self.assertIn("| 3 | 1 | 0 | 0 |", text)
        self.assertIn("- `tools/site/src/page.ts:7:5: ConditionalExpression → true`", text)
        self.assertIn("Report only", text)

    def test_long_lists_are_cut(self) -> None:
        parsed = sr.parse_report(report(*(mutant("Survived", i + 1) for i in range(sr.MAX_LISTED + 3))), "tools/site")
        text = sr.render("demo", parsed, mutated=True)
        self.assertEqual(text.count("\n- `"), sr.MAX_LISTED)
        self.assertIn("and 3 more (see the `mutants-demo` artifact)", text)

    def test_annotations_are_escaped(self) -> None:
        out = sr.annotations([("tools/site/a,b:c.ts", 3, "x\ny%z::warning::w")])
        self.assertEqual(
            out,
            "::warning file=tools/site/a%2Cb%3Ac.ts,line=3,title=Mutation testing::Missed mutant: x%0Ay%25z::warning::w\n",
        )
        self.assertEqual(out.count("\n"), 1)
        self.assertEqual(sr.annotations([("a.ts", i + 1, "m") for i in range(sr.MAX_ANNOTATIONS + 5)]).count("\n"), sr.MAX_ANNOTATIONS)


class MainTest(unittest.TestCase):
    """The subcommands read files and append to the summary file."""

    def test_lines_and_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            diff = Path(tmp, "pr.diff")
            diff.write_text(DIFF, encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(sr.main(["lines", "--diff", str(diff), "--root", "tools/site"]), 0)
            self.assertEqual("".join(c.args[0] for c in stdout.write.call_args_list), "src/page.ts:2-3,src/page.ts:12-12\n")
            data = Path(tmp, "mutation.json")
            data.write_text(report(mutant("Killed"), mutant("Survived", 7)), encoding="utf-8")
            summary = Path(tmp, "summary.md")
            with mock.patch("sys.stdout") as stdout:
                argv = ["report", "--report", str(data), "--root", "tools/site", "--name", "site", "--summary", str(summary)]
                self.assertEqual(sr.main(argv), 0)
            printed = "".join(c.args[0] for c in stdout.write.call_args_list)
            self.assertIn("::warning file=tools/site/src/page.ts,line=7", printed)
            self.assertIn("**50%** of the 2 mutants", summary.read_text(encoding="utf-8"))
            with mock.patch("sys.stdout") as stdout:
                argv = ["report", "--report", str(Path(tmp, "none.json")), "--root", "tools/site", "--name", "site", "--not-mutated", "--summary", ""]
                self.assertEqual(sr.main(argv), 0)
            self.assertIn("touches no source line", "".join(c.args[0] for c in stdout.write.call_args_list))

    def test_usage(self) -> None:
        with mock.patch("sys.stderr"):
            self.assertEqual(sr.main([]), 2)
            self.assertEqual(sr.main(["nope"]), 2)


class WiringTest(unittest.TestCase):
    """The workflow, the Stryker settings and the Workers stay consistent."""

    def test_workflow(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "mutation-workers.yml").read_text(encoding="utf-8"))
        triggers = workflow.get("on", workflow.get(True))
        self.assertEqual(list(triggers), ["pull_request"])
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        job = workflow["jobs"]["worker"]
        roots = {entry["root"] for entry in job["strategy"]["matrix"]["include"]}
        self.assertEqual(roots, {"tools/site", "tools/demo", "tools/diagnostics-collector"})
        for root in roots:
            self.assertIn(f"{root}/**", triggers["pull_request"]["paths"])
            self.assertTrue((ROOT / root / "vitest.config.ts").is_file(), root)
        steps = {s.get("name"): s for s in job["steps"]}
        self.assertFalse(job["steps"][0]["with"]["persist-credentials"])
        self.assertNotIn("ref", job["steps"][0]["with"])
        self.assertIn("git diff HEAD^1 HEAD", steps["Find the changed lines"]["run"])
        self.assertIn("npm install --no-save", steps["Install dependencies and Stryker"]["run"])
        run = steps["Test the mutants"]["run"]
        self.assertIn('--mutate "$MUTATE"', run)
        self.assertIn("../../scripts/stryker.config.json", run)
        self.assertRegex(workflow["env"]["STRYKER_VERSION"], r"^\d+\.\d+\.\d+$")
        self.assertIn("python3 scripts/stryker_report.py report", steps["Report"]["run"])

    def test_settings(self) -> None:
        config = json.loads((ROOT / "scripts" / "stryker.config.json").read_text(encoding="utf-8"))
        self.assertEqual(config["testRunner"], "vitest")
        # In place: tests read repository files by relative path.
        self.assertTrue(config["inPlace"])
        # TypeScript 7 has no JavaScript API for Stryker's tsconfig rewriting.
        self.assertFalse((ROOT / "tools" / "site" / config["tsconfigFile"]).exists())
        # The Workers pool loads modules inside workerd, out of Vitest's module graph.
        self.assertFalse(config["vitest"]["related"])
        self.assertEqual(config["jsonReporter"]["fileName"], "reports/mutation/mutation.json")


if __name__ == "__main__":
    unittest.main()
