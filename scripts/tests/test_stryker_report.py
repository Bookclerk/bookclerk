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
                     "node_modules/x/a.ts", "coverage/a.ts", "a.js", "README.md", "", "a.spec.ts", "src/__tests__/a.ts"):
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


    def test_deletions_mark_the_line_before(self) -> None:
        diff = (
            "diff --git a/tools/site/src/page.ts b/tools/site/src/page.ts\n"
            "--- a/tools/site/src/page.ts\n+++ b/tools/site/src/page.ts\n"
            "@@ -4,4 +4,2 @@\n keep();\n-if (x) {\n-  throw e;\n next();\n"
            "@@ -1,2 +1,1 @@\n-import a from 'a';\n first();\n"
            "@@ -20,2 +18,2 @@\n-old();\n+new();\n same();\n"
        )
        # A replaced line is covered by its new text; a pure deletion marks
        # the line before it, or the first line at the top of the file.
        self.assertEqual(sr.changed_lines(diff, "tools/site"), {"src/page.ts": [(1, 1), (4, 4), (18, 18)]})

    def test_fuzz_deletions_match_git(self) -> None:
        """Oracle: deleting unique lines marks the kept line before each deleted run."""
        rng = random.Random(1582)
        for _ in range(40):
            before = [f"base {i}" for i in range(rng.randrange(1, 30))]
            deleted = {i for i in range(len(before)) if rng.random() < 0.2}
            after = [text for i, text in enumerate(before) if i not in deleted]
            expected: set[int] = set()
            kept = 0
            for i in range(len(before)):
                if i in deleted:
                    if i == 0 or i - 1 not in deleted:
                        expected.add(max(kept, 1))
                else:
                    kept += 1
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
            self.assertEqual({line for start, end in ranges for line in range(start, end + 1)}, expected)


class WidenTest(unittest.TestCase):
    """A file whose changed lines gave no mutant is sampled in a second pass."""

    SOURCE = "".join(f"const v{i} = {i};\n" if i % 5 else "\n" for i in range(1, 101)) + "import x from 'x';\n// note\n"

    def test_parse_mutate_round_trips(self) -> None:
        ranges = {"src/a.ts": [(1, 3), (9, 9)], "b.ts": [(2, 2)]}
        self.assertEqual(sr.parse_mutate(sr.mutate_patterns(ranges)), ranges)
        self.assertEqual(sr.parse_mutate("a b.ts:1-2,x.ts:3-1,y.ts:0-1,z.ts,ok.ts:4-5\n"), {"ok.ts": [(4, 5)]})

    def test_mutant_lines(self) -> None:
        self.assertEqual(sr.mutant_lines(report(mutant("Killed", 3), mutant("CompileError", 7), {"location": None})), {"src/page.ts": {3, 7}})
        self.assertIsNone(sr.mutant_lines("not json"))
        self.assertIsNone(sr.mutant_lines("[]"))

    def test_only_files_without_mutants_are_widened(self) -> None:
        ranges = {"a.ts": [(2, 2)], "b.ts": [(3, 3)]}
        sources = {"a.ts": self.SOURCE, "b.ts": self.SOURCE}
        chosen = sr.widen(ranges, {"a.ts": {2}, "b.ts": {50}}, sources, seed=7)
        self.assertEqual(list(chosen), ["b.ts"])
        self.assertEqual(len(chosen["b.ts"]), sr.WIDEN_BUDGET)
        lines = self.SOURCE.splitlines()
        for line in chosen["b.ts"]:
            self.assertNotEqual(line, 3)
            self.assertIsNone(sr.SKIP_LINE.match(lines[line - 1]), lines[line - 1])
        self.assertEqual(sr.widen(ranges, {"a.ts": {2}, "b.ts": {3}}, sources, seed=7), {})
        # A file that cannot be read is not widened.
        self.assertEqual(sr.widen({"c.ts": [(1, 1)]}, {}, {}, seed=1), {})

    def test_every_file_gets_a_line_and_the_budget_holds(self) -> None:
        for seed in range(60):
            ranges = {f"f{i}.ts": [(1, 1)] for i in range(5)}
            sources = {path: self.SOURCE for path in ranges}
            chosen = sr.widen(ranges, {}, sources, seed=seed, budget=12)
            self.assertEqual(sorted(chosen), sorted(ranges))
            self.assertEqual(sum(len(v) for v in chosen.values()), 12)
            for lines in chosen.values():
                self.assertEqual(len(lines), len(set(lines)))
        # A budget smaller than the file count reaches as many files as it can.
        chosen = sr.widen({f"f{i}.ts": [(1, 1)] for i in range(5)}, {}, {f"f{i}.ts": self.SOURCE for i in range(5)}, seed=3, budget=2)
        self.assertEqual(sum(len(v) for v in chosen.values()), 2)

    def test_small_files_are_taken_whole(self) -> None:
        source = "const a = 1;\n\nconst b = 2;\n"
        self.assertEqual(sr.widen({"s.ts": [(2, 2)]}, {}, {"s.ts": source}, seed=5), {"s.ts": [1, 3]})

    def test_the_sample_moves_with_the_seed(self) -> None:
        ranges, sources = {"b.ts": [(3, 3)]}, {"b.ts": self.SOURCE}
        samples = {tuple(sr.widen(ranges, {}, sources, seed=seed, budget=5)["b.ts"]) for seed in range(20)}
        self.assertGreater(len(samples), 10)

    def test_widen_command(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "src" / "page.ts").write_text(self.SOURCE, encoding="utf-8")
            data = root / "mutation.json"
            data.write_text(report(mutant("Killed", 50)), encoding="utf-8")
            listing = root / "widened.txt"
            argv = ["widen", "--report", str(data), "--mutate", "src/page.ts:3-3", "--root", str(root), "--seed", "582", "--list", str(listing)]
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(sr.main(argv), 0)
            printed = "".join(c.args[0] for c in stdout.write.call_args_list)
            ranges = sr.parse_mutate(printed)
            # The first pass's lines come along, so their results carry over.
            self.assertEqual(ranges["src/page.ts"][0], (3, 3))
            self.assertEqual(len(ranges["src/page.ts"]), 1 + sr.WIDEN_BUDGET)
            self.assertEqual(sr.parse_widened(listing.read_text(encoding="utf-8")), {"src/page.ts": sr.WIDEN_BUDGET})
            # A mutant on the changed line: no second pass.
            data.write_text(report(mutant("Survived", 3)), encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(sr.main(argv), 0)
            self.assertEqual("".join(c.args[0] for c in stdout.write.call_args_list), "")
            # No report: no second pass.
            data.unlink()
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(sr.main(argv), 0)
            self.assertEqual("".join(c.args[0] for c in stdout.write.call_args_list), "")

    def test_sample_lines_skip_what_stryker_never_mutates(self) -> None:
        source = "\n".join(
            [
                "import {",  # 1
                "  a,",  # 2
                "  b,",  # 3
                "} from './x';",  # 4
                "export interface Thing {",  # 5
                "  id: string;",  # 6
                "  nested: { deep: number };",  # 7
                "}",  # 8
                "export type Pair = [number, number];",  # 9
                "const limit = 3;",  # 10
                "function f(x: number) {",  # 11
                "  if (x > limit) {",  # 12
                "    return 'big';",  # 13
                "  }",  # 14
                "  // note",  # 15
                "  return g(x);",  # 16
                "}",  # 17
                "export { f };",  # 18
                "",  # 19
            ]
        )
        self.assertEqual(sr.sample_lines(source), [10, 11, 12, 13, 16])

    def test_sample_lines_skip_multiline_types(self) -> None:
        source = "\n".join(
            [
                "type TokenKind =",  # 1
                '  | "keyword"',  # 2
                '  | "string";',  # 3
                "interface Big",  # 4
                "  extends Base {",  # 5
                "  id: string;",  # 6
                "}",  # 7
                "type Obj = {",  # 8
                "  a: number;",  # 9
                "};",  # 10
                "const limit = 3;",  # 11
                "type One = 'a' | 'b';",  # 12
                "const other = limit + 1;",  # 13
                "const theme = {",  # 14
                '  type: "light",',  # 15: a property named type, not a declaration
                '  fg: "#000",',  # 16
                "};",  # 17
            ]
        )
        self.assertEqual(sr.sample_lines(source), [11, 13, 14, 15, 16])

    def test_ignored_mutants_do_not_cover_a_change(self) -> None:
        """Stryker lists mutants outside --mutate as Ignored; an ignored block around a type change tests nothing."""
        block = mutant("Ignored", 2)
        block["location"] = {"start": {"line": 2, "column": 1}, "end": {"line": 9, "column": 2}}
        self.assertEqual(sr.mutant_lines(report(block, mutant("Survived", 12))), {"src/page.ts": {12}})
        self.assertEqual(list(sr.widen({"a.ts": [(5, 5)]}, {"a.ts": {12}}, {"a.ts": self.SOURCE}, seed=1)), ["a.ts"])

    def test_a_mutant_spanning_a_changed_line_counts(self) -> None:
        spanning = mutant("Killed", 3)
        spanning["location"] = {"start": {"line": 3, "column": 1}, "end": {"line": 6, "column": 2}}
        self.assertEqual(sr.mutant_lines(report(spanning)), {"src/page.ts": {3, 4, 5, 6}})
        self.assertEqual(sr.widen({"a.ts": [(5, 5)]}, {"a.ts": {3, 4, 5, 6}}, {"a.ts": self.SOURCE}, seed=1), {})
        backwards = mutant("Killed", 9)
        backwards["location"] = {"start": {"line": 9, "column": 1}, "end": {"line": 2, "column": 1}}
        self.assertEqual(sr.mutant_lines(report(backwards)), {"src/page.ts": {9}})

    def test_parse_widened(self) -> None:
        self.assertEqual(sr.parse_widened("a.ts\t4\nbad path.ts\t1\nb.ts\tx\nc.ts\t9999999\n"), {"a.ts": 4})


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
        unviable = sr.render("site", sr.parse_report(report(mutant("CompileError")), "tools/site"), mutated=True)
        self.assertIn("none could be tested", unviable)
        self.assertNotIn("found no mutant", unviable)
        parsed = sr.parse_report(report(mutant("Killed"), mutant("Killed"), mutant("Killed"), mutant("Survived", 7)), "tools/site")
        self.assertIn("reached the time limit", sr.render("site", parsed, mutated=True, stopped=True))
        text = sr.render("site", parsed, mutated=True)
        self.assertNotIn("time limit", text)
        self.assertTrue(text.startswith("## Mutation testing (site Worker)\n"))
        self.assertIn("**75%** of the 4 mutants", text)
        self.assertIn("| 3 | 1 | 0 | 0 |", text)
        self.assertIn("- `tools/site/src/page.ts:7:5: ConditionalExpression → true`", text)
        self.assertIn("Report only", text)

    def test_widened_files_are_named(self) -> None:
        results = sr.Results()
        results.mutants[Outcome.CAUGHT] = ["c"]
        summary = sr.render("site", (results, []), mutated=True, widened={"tools/site/src/types.ts": 12})
        self.assertIn("in those files was mutated too, chosen afresh for each pull request: `tools/site/src/types.ts` (12 lines).", summary)
        self.assertIn("of the 1 mutants tested", summary)
        skipped = sr.render("site", (results, []), mutated=True, unwidened=["tools/site/src/types.ts"])
        self.assertIn("`tools/site/src/types.ts`, and the sample of their other lines was not tested", skipped)

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
        self.assertIn('stryker "${limit}s" "$MUTATE"', run)
        self.assertIn("python3 ../../scripts/stryker_report.py widen", run)
        self.assertIn('--seed "$PR"', run)
        self.assertIn('stryker "${left}s" "$widened"', run)
        self.assertEqual(steps["Test the mutants"]["env"]["PR"], "${{ github.event.pull_request.number }}")
        self.assertIn('--widened "$RUNNER_TEMP/widened.txt" --unwidened "$RUNNER_TEMP/unwidened.txt"', steps["Report"]["run"])
        self.assertIn('mv "$RUNNER_TEMP/widened.txt" "$RUNNER_TEMP/unwidened.txt"', run)
        # SIGINT to Stryker alone: the whole process group would stop its
        # test runners before it saves the partial report.
        self.assertIn("timeout --foreground --signal=INT", run)
        self.assertIn('node_modules/.bin/stryker run ../../scripts/stryker.config.json --mutate "$2"', run)
        self.assertRegex(workflow["env"]["STRYKER_VERSION"], r"^\d+\.\d+\.\d+$")
        self.assertIn("python3 scripts/stryker_report.py report", steps["Report"]["run"])
        self.assertIn("--stopped", steps["Report"]["run"])
        self.assertEqual(steps["Report"]["env"]["STOPPED"], "${{ steps.mutants.outputs.stopped }}")
        self.assertEqual(steps["Test the mutants"]["id"], "mutants")

    def test_settings(self) -> None:
        config = json.loads((ROOT / "scripts" / "stryker.config.json").read_text(encoding="utf-8"))
        self.assertEqual(config["testRunner"], "vitest")
        # In place: tests read repository files by relative path.
        self.assertTrue(config["inPlace"])
        # TypeScript 7 has no JavaScript API for Stryker's tsconfig rewriting.
        self.assertFalse((ROOT / "tools" / "site" / config["tsconfigFile"]).exists())
        # The Workers pool loads modules inside workerd, out of Vitest's module graph.
        self.assertFalse(config["vitest"]["related"])
        # Incremental mode is what writes a partial report when the workflow
        # stops Stryker; the report step reads that file.
        self.assertTrue(config["incremental"])
        self.assertEqual(config["incrementalFile"], "reports/mutation/mutation.json")
        self.assertNotIn("json", config["reporters"])


if __name__ == "__main__":
    unittest.main()
