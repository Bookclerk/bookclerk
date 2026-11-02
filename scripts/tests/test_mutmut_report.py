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


class Colour(Enum):
    RED = 1

    def label(self):
        return self.name.lower()


class Box:
    """A box."""

    size = 4

    def grow(self, n):
        return self.size + n

    @staticmethod
    def make():
        return Box()

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
        T = mr.Targets
        cases = {
            "return a + LIMIT": T(["x_first"], False, []),
            '"""Docstring."""': T(["x_first"], False, []),
            # mutmut skips decorated functions and enum methods, but not a
            # bare @staticmethod or @classmethod.
            "@decorator": T([], False, ["second"]),
            "return b * 2": T([], False, ["second"]),
            "return self.name.lower()": T([], False, ["Colour.label"]),
            "return Box()": T(["xǁBoxǁmake"], False, []),
            "return self.size + n": T(["xǁBoxǁgrow"], False, []),
            "return self.size - n": T(["xǁBoxǁshrink"], False, []),
            "size = 4": T([], True, []),
            "RED = 1": T([], True, []),
            "class Box:": T([], True, []),
            "LIMIT = 3": T([], True, []),
            "import os": T([], True, []),
            '"""Module docstring."""': T([], False, []),
            '"""A box."""': T([], False, []),
            "# A comment between functions.": T([], False, []),
        }
        for line, expected in cases.items():
            with self.subTest(line=line):
                self.assertEqual(mr.targets(SOURCE, {L[line]}), expected)
        both = mr.targets(SOURCE, {L["return a + LIMIT"], L["return self.size + n"]})
        self.assertEqual(both, T(["x_first", "xǁBoxǁgrow"], False, []))
        self.assertEqual(mr.targets(SOURCE, set()), T([], False, []))
        self.assertEqual(mr.targets(SOURCE, {10_000}), T([], False, []))

    def test_blank_lines_widen_nothing(self) -> None:
        blank = [i + 1 for i, line in enumerate(SOURCE.splitlines()) if not line.strip()]
        self.assertEqual(mr.targets(SOURCE, set(blank)), mr.Targets([], False, []))

    def test_unparsable_or_unusual_names_widen(self) -> None:
        self.assertTrue(mr.targets("def broken(:\n", {1}).widen)
        self.assertTrue(mr.targets("def café():\n    return 1\n", {2}).widen)
        self.assertFalse(mr.targets("import enum\nclass E(enum.IntEnum):\n    def f(self):\n        return 1\n", {4}).names)

    def test_deleted_lines_are_placed_in_the_old_file(self) -> None:
        diff = (
            f"diff --git a/{SDK}/src/pkg/old.py b/{SDK}/src/pkg/mod.py\n--- a/{SDK}/src/pkg/old.py\n+++ b/{SDK}/src/pkg/mod.py\n"
            f"@@ -{L['return self.size + n']},3 +{L['return self.size + n']},2 @@\n"
            "         return self.size + n\n-        if n:\n-            raise ValueError\n \n"
            "@@ -1,2 +1,1 @@\n-import sys\n \"\"\"Module docstring.\"\"\"\n"
            "@@ -0,0 +3,1 @@\n+import os\n"
        )
        points = mr.changed_points(diff, SDK, GENERATED)
        grow = L["return self.size + n"]
        self.assertEqual(points, {"src/pkg/mod.py": mr.Change({3}, {grow + 1, grow + 2, 1}, "src/pkg/old.py")})

    def test_deletions_select_what_held_them(self) -> None:
        """A deletion selects the function it was in, even when it ends that function."""
        source = SOURCE.splitlines()

        def removed(after: str, *extra: str) -> list[str]:
            index = source.index(after) + 1
            return [*source[:index], *extra, *source[index:]]

        cases = {
            # The function's last lines: a blank line follows in the new file.
            "end of a method": (removed("        return self.size + n", "        if n:", "            raise ValueError"), ["pkg.mod.xǁBoxǁgrow__mutmut_*"], []),
            "a module constant": (removed("LIMIT = 3", "MAX = 9"), ["pkg.mod.x_*", "pkg.mod.xǁ*"], []),
            "a class attribute": (removed("    size = 4", "    colour = 1"), ["pkg.mod.x_*", "pkg.mod.xǁ*"], []),
            "a decorated function's line": (removed("    return b * 2", "    b += 1"), [], ["pkg.mod.second"]),
            "a whole function": (removed("    return a + LIMIT", "", "", "def gone():", "    return 1"), [], []),
            "blank lines": (removed("    return a + LIMIT", ""), [], []),
        }
        for name, (before, patterns, skipped) in cases.items():
            with self.subTest(name):
                old = "".join(t + "\n" for t in before)
                diff = self._diff(before, source, 3, "mod.py")
                self.assertEqual(mr.select(diff, SDK, {"src/pkg/mod.py": SOURCE}, GENERATED, {"src/pkg/mod.py": old}), (patterns, skipped))
        # Without the old file, a deletion selects nothing.
        diff = self._diff(cases["end of a method"][0], source, 3, "mod.py")
        self.assertEqual(mr.select(diff, SDK, {"src/pkg/mod.py": SOURCE}, GENERATED), ([], []))

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
            (["pkg.x_*", "pkg.xǁ*", "pkg.mod.x_first__mutmut_*"], []),
        )
        # A deleted file has nothing to read.
        self.assertEqual(mr.select(DIFF, SDK, {}, GENERATED), ([], []))
        skip = DIFF.replace(f"+{L['return a + LIMIT']},1 @@\n-    return a\n+    return a + LIMIT", f"+{L['return b * 2']},1 @@\n-    return b\n+    return b * 2")
        self.assertEqual(mr.select(skip, SDK, read, GENERATED), (["pkg.x_*", "pkg.xǁ*"], ["pkg.mod.second"]))

    def test_generated_modules_match_the_ruff_exclusions(self) -> None:
        text = (ROOT / SDK / "pyproject.toml").read_text(encoding="utf-8")
        generated = mr.generated_modules(text)
        self.assertEqual(generated, frozenset(tomllib.loads(text)["tool"]["ruff"]["extend-exclude"]))
        for path in generated:
            self.assertTrue((ROOT / SDK / path).is_file(), path)
        self.assertEqual(mr.generated_modules("not toml ["), frozenset())
        self.assertEqual(mr.generated_modules("[tool]\nmutmut = 1\n"), frozenset())
        self.assertEqual(mr.generated_modules("[tool.mutmut]\ndo_not_mutate = 'x'\n"), frozenset())
        # Entries are globs, as mutmut reads them.
        self.assertFalse(mr.is_source("src/pkg/gen/a.py", frozenset({"src/pkg/gen/*"})))
        self.assertTrue(mr.is_source("src/pkg/a.py", frozenset({"src/pkg/gen/*"})))

    def test_fuzz_patterns_round_trip(self) -> None:
        """Oracle: every pattern select writes parses back to the module and function it was made from."""
        rng = random.Random(579)
        idents = ["a", "first", "_private", "Box", "grow", "__init__", "x", "x_y"]
        for _ in range(300):
            module = ".".join(rng.choice(idents) for _ in range(rng.randrange(1, 4)))
            names = sorted({("x_" + rng.choice(idents)) if rng.random() < 0.5 else f"xǁ{rng.choice(idents)}ǁ{rng.choice(idents)}" for _ in range(rng.randrange(0, 4))})
            widen = rng.random() < 0.3
            parsed = mr.parse_patterns("\n".join(mr.patterns_for(module, mr.Targets(names, widen, []))))
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
            found = mr.targets(text, lines)
            self.assertIsInstance(found.widen, bool)
            for name in found.names:
                self.assertRegex(name, r"^x(_|ǁ)[A-Za-z0-9_ǁ]+$")


    def _diff(self, before: list[str], after: list[str], context: int, name: str = "w.py") -> str:
        """Diff two versions of ``src/pkg/<name>`` with git."""
        with tempfile.TemporaryDirectory() as tmp:
            old, new = Path(tmp, "old.py"), Path(tmp, "new.py")
            old.write_text("".join(t + "\n" for t in before), encoding="utf-8")
            new.write_text("".join(t + "\n" for t in after), encoding="utf-8")
            result = subprocess.run(
                ["git", "diff", "--no-index", f"-U{context}", "--", str(old), str(new)],
                capture_output=True,
                text=True,
                check=False,
            )
        return result.stdout.replace(f"+++ b{new}", f"+++ b/{SDK}/src/pkg/{name}").replace(f"+++ {new}", f"+++ b/{SDK}/src/pkg/{name}")

    def test_fuzz_insertions_match_git(self) -> None:
        """Oracle: unique lines inserted at known places are what git's diff adds."""
        rng = random.Random(3579)
        for _ in range(40):
            before = [f"base {i}" for i in range(rng.randrange(0, 30))]
            after = list(before)
            for marker in range(rng.randrange(1, 8)):
                after.insert(rng.randrange(0, len(after) + 1), f"inserted {marker}")
            expected = {i + 1 for i, text in enumerate(after) if text.startswith("inserted")}
            got = mr.changed_points(self._diff(before, after, rng.randrange(0, 4)), SDK, GENERATED)["src/pkg/w.py"]
            self.assertEqual((got.added, got.deleted), (expected, set()))

    def test_fuzz_deletions_match_git(self) -> None:
        """Oracle: deleting unique lines marks exactly those old lines."""
        rng = random.Random(4579)
        for _ in range(40):
            before = [f"base {i}" for i in range(rng.randrange(1, 30))]
            deleted = {i for i in range(len(before)) if rng.random() < 0.2} or {0}
            after = [text for i, text in enumerate(before) if i not in deleted]
            got = mr.changed_points(self._diff(before, after, rng.randrange(0, 4)), SDK, GENERATED)["src/pkg/w.py"]
            self.assertEqual((got.added, got.deleted), (set(), {i + 1 for i in deleted}))


class ResultsTest(unittest.TestCase):
    """mutmut's exit codes map onto the Rust job's outcomes, scoped by the patterns."""

    def test_selected_is_exact(self) -> None:
        patterns = mr.parse_patterns("pkg.x_*\npkg.xǁ*\npkg.mod.x_first__mutmut_*\nnot a pattern\npkg.x*\n")
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

    def test_a_meta_file_cut_short_is_still_read(self) -> None:
        codes = {f"a.x_f__mutmut_{n}": (n % 3 or None) for n in range(1, 8)}
        text = json.dumps({"exit_code_by_key": codes, "durations_by_key": {"a.x_f__mutmut_1": 2}}, indent=4)
        self.assertEqual(mr.parse_meta(text), codes)
        cut = text[: text.index("durations_by_key") + 30]
        self.assertEqual(mr.parse_meta(cut), codes)
        partial = text[: text.index("a.x_f__mutmut_5")]
        self.assertEqual(mr.parse_meta(partial), {k: v for k, v in codes.items() if int(k.rsplit("_", 1)[1]) < 5})
        # json.dump escapes a method key's separators; the salvage decodes them.
        methods = {f"a.xǁBoxǁgrow__mutmut_{n}": n for n in range(1, 4)}
        text = json.dumps({"exit_code_by_key": methods, "durations_by_key": {}}, indent=4)
        self.assertIn("\\u01c1", text)
        self.assertEqual(mr.parse_meta(text[: text.index("durations_by_key")]), methods)

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
        patterns = mr.parse_patterns("pkg.mod.x_first__mutmut_*")
        loads: list[str] = []

        def load(path: str) -> tuple[str, str]:
            loads.append(path)
            return SOURCE, mutated

        results, survivors, other = mr.collect({"src/pkg/mod.py": codes, "src/pkg/other.py": {"pkg.other.x_g__mutmut_1": 0}}, load, patterns, SDK)
        # Exit code 34 is not one the outcomes count.
        self.assertEqual(other, 1)
        # Only a module with a selected missed mutant is read.
        self.assertEqual(loads, ["src/pkg/mod.py"])
        self.assertEqual(mr.progress({"src/pkg/mod.py": codes}, patterns), (6, 5))
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
            parsed_codes = mr.parse_meta(json.dumps({"exit_code_by_key": codes}))
            results, survivors, _ = mr.collect({"src/pkg/mod.py": parsed_codes}, lambda _: (original, mutated), [("pkg.mod", None)], SDK)
            for descriptions in results.mutants.values():
                for description in descriptions:
                    self.assertLessEqual(len(description), sys.modules["mutation_report"].MAX_LINE)
                    self.assertTrue(description.isprintable(), description)
            for _, line, _ in survivors:
                self.assertGreater(line, 0)


class RenderTest(unittest.TestCase):
    """The summary says what happened in every state."""

    def results(self, caught: int, missed: int, other: int = 0) -> tuple[mr.Results, list[tuple[str, int, str]], int]:
        results = mr.Results()
        results.mutants[Outcome.CAUGHT] = [f"c{i}" for i in range(caught)]
        results.mutants[Outcome.MISSED] = [f"m{i}" for i in range(missed)]
        return results, [], other

    def test_states(self) -> None:
        patterns = [("pkg.mod", "x_first")]
        self.assertIn("touches no source line", mr.render(None, patterns=[]))
        self.assertIn("did not test any mutant of the changed functions; check the job log", mr.render(None, patterns=patterns))
        self.assertIn("reached the time limit first", mr.render(None, patterns=patterns, stopped=True))
        self.assertIn("cannot mutate any of the changed functions", mr.render(None, patterns=[], skipped=["pkg.mod.second"]))
        skipped = mr.render(self.results(1, 0), patterns=patterns, skipped=["pkg.mod.second"])
        self.assertIn("- `pkg.mod.second`", skipped)
        self.assertIn("no mutant in the changed functions", mr.render(self.results(0, 0), patterns=patterns))
        summary = mr.render(self.results(3, 1), patterns=patterns)
        self.assertIn("**75%** of the 4 mutants", summary)
        self.assertIn("- `m0`", summary)
        self.assertIn("Report only", summary)
        self.assertIn("time limit", mr.render(self.results(1, 0), patterns=patterns, stopped=True))
        widened = mr.render(self.results(1, 0), patterns=[("pkg.mod", None)])
        self.assertIn("outside any function in `pkg.mod`, so all of their mutable functions", widened)
        # Mutants that ended with a code the outcomes do not count are not "no mutant".
        self.assertIn("None of the 2 mutants mutmut ran", mr.render(self.results(0, 0, 2), patterns=patterns))
        self.assertIn("1 more ended without a result", mr.render(self.results(1, 0, 1), patterns=patterns))

    def test_long_lists_are_cut(self) -> None:
        summary = mr.render(self.results(0, mr.MAX_LISTED + 5), patterns=[("pkg", None)])
        self.assertIn("… and 5 more (see the `mutants-python-sdk` artifact)", summary)


class MainTest(unittest.TestCase):
    """The subcommands read files and append to the summary file."""

    def test_select_places_deletions_at_the_base(self) -> None:
        """``--base`` reads the old file from git, so deleting a method's last lines selects it."""
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            module = repo / "sdk" / "src" / "pkg" / "mod.py"
            module.parent.mkdir(parents=True)
            old = SOURCE.replace("        return self.size + n\n", "        return self.size + n\n        if n:\n            raise ValueError\n")
            module.write_text(old, encoding="utf-8")
            git = ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"]
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run([*git, "-C", str(repo), "add", "."], check=True)
            subprocess.run([*git, "-C", str(repo), "commit", "-qm", "base"], check=True)
            module.write_text(SOURCE, encoding="utf-8")
            diff = Path(tmp, "pr.diff")
            diff.write_text(subprocess.run(["git", "-C", str(repo), "diff"], capture_output=True, text=True, check=True).stdout, encoding="utf-8")
            cwd = Path.cwd()
            os.chdir(repo)
            try:
                with mock.patch("sys.stdout") as stdout:
                    self.assertEqual(mr.main(["select", "--diff", str(diff), "--root", "sdk", "--base", "HEAD"]), 0)
                printed = "".join(c.args[0] for c in stdout.write.call_args_list)
                with mock.patch("sys.stdout") as stdout:
                    self.assertEqual(mr.main(["select", "--diff", str(diff), "--root", "sdk"]), 0)
                without = "".join(c.args[0] for c in stdout.write.call_args_list)
                self.assertIsNone(mr._git_show("-p", "sdk/src/pkg/mod.py"))
                self.assertIsNone(mr._git_show("HEAD", "../outside.py"))
                self.assertIsNone(mr._git_show("HEAD", "sdk/missing.py"))
            finally:
                os.chdir(cwd)
            self.assertEqual(printed, "pkg.mod.xǁBoxǁgrow__mutmut_*\n")
            self.assertEqual(without, "")

    def test_select_and_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "sdk")
            (root / "src" / "pkg").mkdir(parents=True)
            (root / "src" / "pkg" / "mod.py").write_text(SOURCE, encoding="utf-8")
            (root / "pyproject.toml").write_text('[tool.mutmut]\ndo_not_mutate = ["src/pkg/abi.py"]\n', encoding="utf-8")
            diff = Path(tmp, "pr.diff")
            diff.write_text(DIFF.replace(f"{SDK}/", f"{root.as_posix()}/"), encoding="utf-8")
            skipped = Path(tmp, "skipped.txt")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["select", "--diff", str(diff), "--root", root.as_posix(), "--skipped", str(skipped)]), 0)
            printed = "".join(c.args[0] for c in stdout.write.call_args_list)
            self.assertEqual(printed, "pkg.mod.x_first__mutmut_*\n")
            self.assertEqual(skipped.read_text(encoding="utf-8"), "")
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
            self.assertIn("did not test any mutant", "".join(c.args[0] for c in stdout.write.call_args_list))
            # Results written but none tested: mutmut failed before testing.
            (mutants / "src" / "pkg" / "mod.py.meta").write_text(json.dumps({"exit_code_by_key": {"pkg.mod.x_first__mutmut_1": None}}), encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["report", "--mutants", str(mutants), "--patterns", str(patterns), "--root", root.as_posix(), "--summary", ""]), 0)
            self.assertIn("did not test any mutant", "".join(c.args[0] for c in stdout.write.call_args_list))
            # No selected mutant at all: the changed functions had none.
            other = Path(tmp, "other.txt")
            other.write_text("pkg.mod.x_nothing__mutmut_*\n", encoding="utf-8")
            with mock.patch("sys.stdout") as stdout:
                self.assertEqual(mr.main(["report", "--mutants", str(mutants), "--patterns", str(other), "--root", root.as_posix(), "--summary", ""]), 0)
            self.assertIn("found no mutant in the changed functions", "".join(c.args[0] for c in stdout.write.call_args_list))

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
        # SIGINT to the whole process group, mutmut's test workers included.
        self.assertIn("timeout --signal=INT", run)
        self.assertNotIn("--foreground", run)
        self.assertEqual(steps["Test the mutants"]["id"], "mutants")
        self.assertEqual(steps["Test the mutants"]["working-directory"], "${{ env.SDK }}")
        self.assertIn("scripts/stryker_report.py", triggers["pull_request"]["paths"])
        self.assertIn("scripts/mutation_report.py", triggers["pull_request"]["paths"])
        self.assertIn('--skipped "$RUNNER_TEMP/skipped.txt"', steps["Find the changed functions"]["run"])
        self.assertIn("steps.select.outcome == 'success'", steps["Report"]["if"])
        report = steps["Report"]["run"]
        self.assertIn("python3 scripts/mutmut_report.py report", report)
        self.assertIn("--stopped", report)
        self.assertIn('--skipped "$RUNNER_TEMP/skipped.txt"', report)
        self.assertEqual(steps["Report"]["env"]["STOPPED"], "${{ steps.mutants.outputs.stopped }}")
        self.assertEqual(steps["Upload the results"]["with"]["name"], "mutants-python-sdk")

    def test_settings(self) -> None:
        config = tomllib.loads((ROOT / SDK / "pyproject.toml").read_text(encoding="utf-8"))["tool"]["mutmut"]
        self.assertEqual(config["source_paths"], ["src/bookclerk_plugin_sdk"])
        self.assertEqual(config["pytest_add_cli_args_test_selection"], ["tests"])
        self.assertIn(f"{SDK}/mutants/", (ROOT / ".gitignore").read_text(encoding="utf-8").splitlines())


if __name__ == "__main__":
    unittest.main()
