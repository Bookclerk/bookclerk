#!/usr/bin/env python3
"""Tests for scripts/mutmut_report.py: function selection, results, summary and wiring.

The diff, the sources and mutmut's files are written in the same job, so they
are not an untrusted-input boundary; the parsers still keep every value
bounded and printable, and seeded fuzz tests check them against independent
readings.
"""

from __future__ import annotations

import ast
import importlib.util
import json
import random
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SDK = "packages/plugin-sdk-python"


def _load() -> ModuleType:
    sys.path.insert(0, str(ROOT / "scripts"))
    spec = importlib.util.spec_from_file_location("mutmut_report", ROOT / "scripts" / "mutmut_report.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutmut_report"] = module
    spec.loader.exec_module(module)
    return module


mr = _load()
Outcome = mr.Outcome

SOURCE = '''\
"""Module docstring."""

import os

LIMIT = 3


def first(a):
    """Docstring."""
    return a + LIMIT


@decorator
def second(b):
    return b * 2


class Box:
    """A box."""

    size = 4

    def grow(self, n):
        return self.size + n

    async def shrink(self, n):
        return self.size - n


# A comment between functions.
'''


def lines_of(text: str) -> dict[str, int]:
    """Map each distinct stripped line of SOURCE to its 1-based number."""
    return {line.strip(): i + 1 for i, line in enumerate(text.splitlines())}


L = lines_of(SOURCE)

DIFF = f"""\
diff --git a/{SDK}/src/pkg/mod.py b/{SDK}/src/pkg/mod.py
--- a/{SDK}/src/pkg/mod.py
+++ b/{SDK}/src/pkg/mod.py
@@ -9,1 +{L["return a + LIMIT"]},1 @@
-    return a
+    return a + LIMIT
diff --git a/{SDK}/src/pkg/abi.py b/{SDK}/src/pkg/abi.py
--- a/{SDK}/src/pkg/abi.py
+++ b/{SDK}/src/pkg/abi.py
@@ -1 +1 @@
-old
+new
diff --git a/{SDK}/tests/test_mod.py b/{SDK}/tests/test_mod.py
--- a/{SDK}/tests/test_mod.py
+++ b/{SDK}/tests/test_mod.py
@@ -1 +1 @@
-old
+new
diff --git a/{SDK}/src/pkg/__init__.py b/{SDK}/src/pkg/__init__.py
--- a/{SDK}/src/pkg/__init__.py
+++ b/{SDK}/src/pkg/__init__.py
@@ -1 +1 @@
-X = 1
+X = 2
"""

GENERATED = frozenset({"src/pkg/abi.py"})


def mutated_module(original: str, base: str, name: str, mutants: dict[int, tuple[str, str]]) -> str:
    """Build a module shaped like mutmut's copy: the original plus renamed copies.

    Args:
        original: The original module.
        base: The mangled function name (``x_first``).
        name: The function's own name in ``original`` (``first``).
        mutants: Per mutant number, ``(old, new)`` text replaced once in the
            function's copy.

    The copies go at the end, at the function's own indentation; SOURCE
    ends with ``Box`` (a comment does not close it), so a method's copies
    stay in its class, as mutmut places them.
    """
    tree = ast.parse(original)
    node = next(n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name == name)
    lines = original.splitlines()
    func = lines[node.lineno - 1 : node.end_lineno]
    out = [*lines, ""]
    copies = {"orig": func, **{str(n): "\n".join(func).replace(old, new, 1).split("\n") for n, (old, new) in mutants.items()}}
    for suffix, body in copies.items():
        out += [body[0].replace(f"def {name}(", f"def {base}__mutmut_{suffix}(", 1), *body[1:], ""]
    return "\n".join(out) + "\n"


class SelectTest(unittest.TestCase):
    """Changed lines become one pattern per changed function, or a whole module."""

    def test_targets(self) -> None:
        cases = {
            "return a + LIMIT": (["x_first"], False),
            '"""Docstring."""': (["x_first"], False),
            "@decorator": (["x_second"], False),
            "return self.size + n": (["xǁBoxǁgrow"], False),
            "return self.size - n": (["xǁBoxǁshrink"], False),
            "size = 4": ([], True),
            "class Box:": ([], True),
            "LIMIT = 3": ([], True),
            "import os": ([], True),
            '"""Module docstring."""': ([], False),
            '"""A box."""': ([], False),
            "# A comment between functions.": ([], False),
        }
        for line, expected in cases.items():
            with self.subTest(line=line):
                self.assertEqual(mr.targets(SOURCE, {L[line]}), expected)
        self.assertEqual(mr.targets(SOURCE, {L["return a + LIMIT"], L["return self.size + n"]}), (["x_first", "xǁBoxǁgrow"], False))
        self.assertEqual(mr.targets(SOURCE, set()), ([], False))
        self.assertEqual(mr.targets(SOURCE, {10_000}), ([], False))

    def test_blank_lines_widen_nothing(self) -> None:
        blank = [i + 1 for i, line in enumerate(SOURCE.splitlines()) if not line.strip()]
        self.assertEqual(mr.targets(SOURCE, set(blank)), ([], False))

    def test_unparsable_or_unusual_names_widen(self) -> None:
        self.assertEqual(mr.targets("def broken(:\n", {1}), ([], True))
        self.assertEqual(mr.targets("def café():\n    return 1\n", {2}), ([], True))

    def test_sources_and_modules(self) -> None:
        self.assertTrue(mr.is_source("src/pkg/mod.py", GENERATED))
        self.assertTrue(mr.is_source("src/pkg/sub/mod.py", GENERATED))
        for path in ("src/pkg/abi.py", "tests/test_mod.py", "src/pkg/tests/t.py", "src/pkg/data.json", "pyproject.toml", "mod.py"):
            with self.subTest(path=path):
                self.assertFalse(mr.is_source(path, GENERATED))
        self.assertEqual(mr.module_name("src/pkg/mod.py"), "pkg.mod")
        self.assertEqual(mr.module_name("src/pkg/__init__.py"), "pkg")
        self.assertEqual(mr.module_name("src/pkg/sub/mod.py"), "pkg.sub.mod")
        self.assertIsNone(mr.module_name("lib/pkg/mod.py"))
        self.assertIsNone(mr.module_name("src/pkg/my-mod.py"))

    def test_select(self) -> None:
        read = {"src/pkg/mod.py": SOURCE, "src/pkg/__init__.py": "X = 2\n"}
        self.assertEqual(
            mr.select(DIFF, SDK, read, GENERATED),
            ["pkg.x*", "pkg.mod.x_first__mutmut_*"],
        )
        # A deleted file has nothing to read.
        self.assertEqual(mr.select(DIFF, SDK, {}, GENERATED), [])

    def test_generated_modules_match_the_ruff_exclusions(self) -> None:
        text = (ROOT / SDK / "pyproject.toml").read_text(encoding="utf-8")
        generated = mr.generated_modules(text)
        self.assertEqual(generated, frozenset(tomllib.loads(text)["tool"]["ruff"]["extend-exclude"]))
        for path in generated:
            self.assertTrue((ROOT / SDK / path).is_file(), path)
        self.assertEqual(mr.generated_modules("not toml ["), frozenset())
        self.assertEqual(mr.generated_modules("[tool]\nmutmut = 1\n"), frozenset())
        self.assertEqual(mr.generated_modules("[tool.mutmut]\ndo_not_mutate = 'x'\n"), frozenset())

    def test_fuzz_patterns_round_trip(self) -> None:
        """Oracle: every pattern select writes parses back to the module and function it was made from."""
        rng = random.Random(579)
        idents = ["a", "first", "_private", "Box", "grow", "__init__", "x", "x_y"]
        for _ in range(300):
            module = ".".join(rng.choice(idents) for _ in range(rng.randrange(1, 4)))
            names = sorted({("x_" + rng.choice(idents)) if rng.random() < 0.5 else f"xǁ{rng.choice(idents)}ǁ{rng.choice(idents)}" for _ in range(rng.randrange(0, 4))})
            widen = rng.random() < 0.3
            parsed = mr.parse_patterns("\n".join(mr.patterns_for(module, names, widen)))
            self.assertEqual(parsed, [(module, None)] if widen else [(module, n) for n in names])
            for module_, base in parsed:
                key = f"{module_}.{base or 'x_any'}__mutmut_{rng.randrange(1, 999)}"
                self.assertTrue(mr.selected(key, parsed), key)

    def test_fuzz_targets_never_raise(self) -> None:
        rng = random.Random(1579)
        alphabet = list("defclass():=\n #\"'@xyz ") + ["\n    ", "def f():\n", "class C:\n"]
        for _ in range(300):
            text = "".join(rng.choice(alphabet) for _ in range(rng.randrange(0, 80)))
            lines = {rng.randrange(0, 12) for _ in range(rng.randrange(0, 4))}
            names, widen = mr.targets(text, lines)
            self.assertIsInstance(widen, bool)
            for name in names:
                self.assertRegex(name, r"^x(_|ǁ)[A-Za-z0-9_ǁ]+$")


class ResultsTest(unittest.TestCase):
    """mutmut's exit codes map onto the Rust job's outcomes, scoped by the patterns."""

    def test_selected_is_exact(self) -> None:
        patterns = mr.parse_patterns("pkg.x*\npkg.mod.x_first__mutmut_*\nnot a pattern\n")
        self.assertEqual(patterns, [("pkg", None), ("pkg.mod", "x_first")])
        self.assertTrue(mr.selected("pkg.x_anything__mutmut_3", patterns))
        self.assertTrue(mr.selected("pkg.mod.x_first__mutmut_12", patterns))
        # A whole-module pattern does not reach into submodules.
        self.assertFalse(mr.selected("pkg.xsub.x_f__mutmut_1", patterns))
        self.assertFalse(mr.selected("pkg.mod.x_first_two__mutmut_1", patterns))
        self.assertFalse(mr.selected("pkg.mod.x_second__mutmut_1", patterns))
        self.assertFalse(mr.selected("pkg.mod.x_first__mutmut_orig", patterns))

    def test_parse_meta(self) -> None:
        meta = json.dumps({"exit_code_by_key": {"a.x_f__mutmut_1": 1, "a.x_f__mutmut_2": None, "a.x_f__mutmut_3": "x"}})
        self.assertEqual(mr.parse_meta(meta), {"a.x_f__mutmut_1": 1, "a.x_f__mutmut_2": None, "a.x_f__mutmut_3": None})
        for bad in ("", "[]", "{}", '{"exit_code_by_key": []}', "{" * 100_000):
            with self.subTest(bad=bad[:20]):
                self.assertEqual(mr.parse_meta(bad), {})

    def test_change_places_the_mutant(self) -> None:
        mutated = mutated_module(SOURCE, "x_first", "first", {1: ("a + LIMIT", "a - LIMIT"), 2: ('"""Docstring."""\n', "")})
        module = mr.Module.read(SOURCE, mutated)
        self.assertEqual(module.change("pkg.mod.x_first__mutmut_1"), (L["return a + LIMIT"], "return a + LIMIT → return a - LIMIT"))
        self.assertEqual(module.change("pkg.mod.x_first__mutmut_9"), (0, ""))
        self.assertEqual(module.change("pkg.mod.x_first__mutmut_orig"), (0, ""))
        method = mutated_module(SOURCE, "xǁBoxǁgrow", "grow", {1: ("+ n", "- n")})
        module = mr.Module.read(SOURCE, method)
        self.assertEqual(module.change("pkg.mod.xǁBoxǁgrow__mutmut_1"), (L["return self.size + n"], "return self.size + n → return self.size - n"))
        # Without the original, the change is still described, unplaced.
        self.assertEqual(mr.Module.read("not python (", method).change("pkg.mod.xǁBoxǁgrow__mutmut_1")[0], 0)

    def test_collect(self) -> None:
        mutated = mutated_module(SOURCE, "x_first", "first", {1: ("a + LIMIT", "a - LIMIT"), 2: ("a + LIMIT", "a + 1")})
        codes = {
            "pkg.mod.x_first__mutmut_1": 0,
            "pkg.mod.x_first__mutmut_2": 1,
            "pkg.mod.x_first__mutmut_3": 36,
            "pkg.mod.x_first__mutmut_4": 33,
            "pkg.mod.x_first__mutmut_5": None,
            "pkg.mod.x_first__mutmut_6": 34,
            "pkg.mod.x_second__mutmut_1": 0,
        }
        metas = {"src/pkg/mod.py": json.dumps({"exit_code_by_key": codes})}
        patterns = mr.parse_patterns("pkg.mod.x_first__mutmut_*")
        results, survivors = mr.collect(metas, {"src/pkg/mod.py": (SOURCE, mutated)}, patterns, SDK)
        self.assertEqual(results.count(Outcome.CAUGHT), 1)
        self.assertEqual(results.count(Outcome.TIMEOUT), 1)
        self.assertEqual(results.count(Outcome.MISSED), 2)
        line = L["return a + LIMIT"]
        self.assertEqual(
            results.mutants[Outcome.MISSED][0],
            f"{SDK}/src/pkg/mod.py:{line}: return a + LIMIT → return a - LIMIT (x_first__mutmut_1)",
        )
        # A mutant no test runs is missed, but cannot be placed without its copy.
        self.assertEqual(results.mutants[Outcome.MISSED][1], f"{SDK}/src/pkg/mod.py: x_first__mutmut_4")
        self.assertEqual(survivors, [(f"{SDK}/src/pkg/mod.py", line, results.mutants[Outcome.MISSED][0])])

    def test_exit_codes_match_mutmut(self) -> None:
        caught = {code for code, outcome in mr.EXIT_CODES.items() if outcome is Outcome.CAUGHT}
        self.assertEqual(caught, {1, 3, 37})
        self.assertNotIn(None, mr.EXIT_CODES)
        self.assertNotIn(34, mr.EXIT_CODES)

    def test_fuzz_descriptions_stay_printable_and_bounded(self) -> None:
        rng = random.Random(2579)
        values = ["", "x", "\x00\n\r", "→" * 400, "def x_f__mutmut_1(\n", "    return 1\n"]
        for _ in range(200):
            original = "".join(rng.choice(values + [SOURCE]) for _ in range(rng.randrange(0, 4)))
            mutated = original + "".join(rng.choice(values) for _ in range(rng.randrange(0, 6)))
            codes = {f"pkg.mod.x_f__mutmut_{n}": rng.choice([0, 1, 33, 36, None, -9, 999]) for n in range(rng.randrange(0, 6))}
            codes[rng.choice(values)] = 0
            meta = json.dumps({"exit_code_by_key": codes})
            results, survivors = mr.collect({"src/pkg/mod.py": meta}, {"src/pkg/mod.py": (original, mutated)}, [("pkg.mod", None)], SDK)
            for descriptions in results.mutants.values():
                for description in descriptions:
                    self.assertLessEqual(len(description), mr.MAX_LINE)
                    self.assertTrue(description.isprintable(), description)
            for _, line, _ in survivors:
                self.assertGreater(line, 0)


class RenderTest(unittest.TestCase):
    """The summary says what happened in every state."""

    def results(self, caught: int, missed: int) -> tuple[mr.Results, list[tuple[str, int, str]]]:
        results = mr.Results()
        results.mutants[Outcome.CAUGHT] = [f"c{i}" for i in range(caught)]
        results.mutants[Outcome.MISSED] = [f"m{i}" for i in range(missed)]
        return results, []

    def test_states(self) -> None:
        patterns = [("pkg.mod", "x_first")]
        self.assertIn("touches no source line", mr.render(None, patterns=[]))
        self.assertIn("did not finish", mr.render(None, patterns=patterns))
        self.assertIn("no mutant in the changed functions", mr.render(self.results(0, 0), patterns=patterns))
        summary = mr.render(self.results(3, 1), patterns=patterns)
        self.assertIn("**75%** of the 4 mutants", summary)
        self.assertIn("- `m0`", summary)
        self.assertIn("Report only", summary)
        self.assertIn("time limit", mr.render(self.results(1, 0), patterns=patterns, stopped=True))
        widened = mr.render(self.results(1, 0), patterns=[("pkg.mod", None)])
        self.assertIn("outside any function in `pkg.mod`", widened)

    def test_long_lists_are_cut(self) -> None:
        summary = mr.render(self.results(0, mr.MAX_LISTED + 5), patterns=[("pkg", None)])
        self.assertIn("… and 5 more (see the `mutants-python-sdk` artifact)", summary)


class MainTest(unittest.TestCase):
    """The subcommands read files and append to the summary file."""

    def test_select_and_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "sdk")
            (root / "src" / "pkg").mkdir(parents=True)
            (root / "src" / "pkg" / "mod.py").write_text(SOURCE, encoding="utf-8")
            (root / "pyproject.toml").write_text('[tool.mutmut]\ndo_not_mutate = ["src/pkg/abi.py"]\n', encoding="utf-8")
            diff = Path(tmp, "pr.diff")
            diff.write_text(DIFF.replace(f"{SDK}/", f"{root.as_posix()}/"), encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["select", "--diff", str(diff), "--root", root.as_posix()]), 0)
            printed = "".join(c.args[0] for c in stdout.write.call_args_list)
            self.assertEqual(printed, "pkg.mod.x_first__mutmut_*\n")
            patterns = Path(tmp, "patterns.txt")
            patterns.write_text(printed, encoding="utf-8")

            mutants = Path(tmp, "mutants")
            (mutants / "src" / "pkg").mkdir(parents=True)
            (mutants / "src" / "pkg" / "mod.py").write_text(mutated_module(SOURCE, "x_first", "first", {1: ("+ LIMIT", "- LIMIT")}), encoding="utf-8")
            meta = {"exit_code_by_key": {"pkg.mod.x_first__mutmut_1": 0, "pkg.mod.x_first__mutmut_2": 1}}
            (mutants / "src" / "pkg" / "mod.py.meta").write_text(json.dumps(meta), encoding="utf-8")
            summary, out = Path(tmp, "summary.md"), Path(tmp, "out")
            argv = ["report", "--mutants", str(mutants), "--patterns", str(patterns), "--root", root.as_posix(), "--summary", str(summary), "--out", str(out)]
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(argv), 0)
            printed = "".join(c.args[0] for c in stdout.write.call_args_list)
            self.assertIn(f"::warning file={root.as_posix().replace(':', '%3A')}/src/pkg/mod.py,line={L['return a + LIMIT']}", printed)
            self.assertIn("**50%** of the 2 mutants", summary.read_text(encoding="utf-8"))
            self.assertIn("x_first__mutmut_1", (out / "missed.txt").read_text(encoding="utf-8"))
            self.assertEqual((out / "unviable.txt").read_text(encoding="utf-8"), "")

            # No patterns: mutmut did not run.
            empty = Path(tmp, "empty.txt")
            empty.write_text("", encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["report", "--mutants", str(mutants), "--patterns", str(empty), "--root", root.as_posix(), "--summary", ""]), 0)
            self.assertIn("touches no source line", "".join(c.args[0] for c in stdout.write.call_args_list))
            # Patterns but no results: mutmut did not get as far as writing them.
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["report", "--mutants", str(Path(tmp, "none")), "--patterns", str(patterns), "--root", root.as_posix(), "--summary", ""]), 0)
            self.assertIn("did not finish", "".join(c.args[0] for c in stdout.write.call_args_list))

    def test_usage(self) -> None:
        with mock.patch("sys.stderr"):
            self.assertEqual(mr.main([]), 2)
            self.assertEqual(mr.main(["nope"]), 2)


class WiringTest(unittest.TestCase):
    """The workflow, the mutmut settings and the SDK stay consistent."""

    def test_workflow(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "mutation-python.yml").read_text(encoding="utf-8"))
        triggers = workflow.get("on", workflow.get(True))
        self.assertEqual(list(triggers), ["pull_request"])
        self.assertIn(f"{SDK}/**", triggers["pull_request"]["paths"])
        self.assertIn("scripts/mutmut_report.py", triggers["pull_request"]["paths"])
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        self.assertEqual(workflow["env"]["SDK"], SDK)
        self.assertRegex(workflow["env"]["MUTMUT_VERSION"], r"^\d+\.\d+\.\d+$")
        job = workflow["jobs"]["mutants"]
        steps = {s.get("name"): s for s in job["steps"]}
        self.assertFalse(job["steps"][0]["with"]["persist-credentials"])
        self.assertNotIn("ref", job["steps"][0]["with"])
        self.assertIn("git diff HEAD^1 HEAD", steps["Find the changed functions"]["run"])
        self.assertIn("--locked", steps["Install the test dependencies and mutmut"]["run"])
        self.assertIn('"mutmut==$MUTMUT_VERSION"', steps["Install the test dependencies and mutmut"]["run"])
        run = steps["Test the mutants"]["run"]
        # Patterns as separate arguments, never split or globbed by the shell.
        self.assertIn('mapfile -t patterns < "$RUNNER_TEMP/patterns.txt"', run)
        self.assertIn('mutmut run "${patterns[@]}"', run)
        self.assertIn("timeout --foreground --signal=INT", run)
        self.assertEqual(steps["Test the mutants"]["id"], "mutants")
        self.assertEqual(steps["Test the mutants"]["working-directory"], "${{ env.SDK }}")
        report = steps["Report"]["run"]
        self.assertIn("python3 scripts/mutmut_report.py report", report)
        self.assertIn("--stopped", report)
        self.assertEqual(steps["Report"]["env"]["STOPPED"], "${{ steps.mutants.outputs.stopped }}")
        self.assertEqual(steps["Upload the results"]["with"]["name"], "mutants-python-sdk")

    def test_settings(self) -> None:
        config = tomllib.loads((ROOT / SDK / "pyproject.toml").read_text(encoding="utf-8"))["tool"]["mutmut"]
        self.assertEqual(config["source_paths"], ["src/bookclerk_plugin_sdk"])
        self.assertEqual(config["pytest_add_cli_args_test_selection"], ["tests"])
        self.assertIn(f"{SDK}/mutants/", (ROOT / ".gitignore").read_text(encoding="utf-8").splitlines())


if __name__ == "__main__":
    unittest.main()
