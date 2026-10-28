#!/usr/bin/env python3
"""coverage_gate.py: areas file, LCOV and diff parsing, patch verdict, summary.

The LCOV reports and the diff come from the pull request's own run, so both
parsers are an untrusted-input boundary (docs/contributing/fuzzing.md). The
seed corpus in `coverage_gate_corpus/` (files named `<kind>__<case>`, kind
`lcov` or `diff`) and seeded mutations of it are read by the module and by
an independent oracle here, and must read the same; LCOV paths must also
stay inside the repository. Everything runs offline; `MainTest` runs the
whole script against a temporary reports directory and diff file, with the
repository's own areas file.
"""

from __future__ import annotations

import importlib.util
import os
import random
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path
from types import ModuleType

import tomllib

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "coverage_gate_corpus"
#: The area root and workspace every corpus report is read with.
CORPUS_ROOT = "tools/site"
CORPUS_WORKSPACE = "/w"


def _load() -> ModuleType:
    path = ROOT / "scripts" / "coverage_gate.py"
    spec = importlib.util.spec_from_file_location("coverage_gate", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["coverage_gate"] = module
    spec.loader.exec_module(module)
    return module


cg = _load()

CONFIG = """
mode = "enforce"
patch_target = 80

[areas.rust]
report = "rust/lcov.info"
paths = ["crates/"]
suffixes = [".rs"]
floor = 50

[areas.site]
report = "site/lcov.info"
root = "tools/site"
paths = ["tools/site/"]
suffixes = [".ts"]
floor = 90
"""


def area(name: str = "rust", floor: float = 0) -> object:
    """Build an area owning ``crates/*.rs``."""
    return cg.Area(name, f"{name}/lcov.info", ".", ("crates/",), (".rs",), floor)


def oracle_path(source: str, root: str, workspace: str) -> str | None:
    """Resolve an ``SF:`` path by walking its segments, refusing any climb out."""
    path = source.replace("\\", "/")
    absolute = path.startswith("/")
    stack: list[str] = []
    for part in (path if absolute else f"{root}/{path}").split("/"):
        if part in ("", "."):
            continue
        if part != "..":
            stack.append(part)
        elif stack:
            stack.pop()
        elif not absolute:
            return None
    if absolute:
        base = [p for p in workspace.split("/") if p]
        if stack[: len(base)] != base:
            return None
        stack = stack[len(base) :]
    return "/".join(stack) or None


def oracle_lcov(text: str, root: str, workspace: str) -> dict[str, dict[int, int]]:
    """Read an LCOV report record by record with full-line patterns."""
    files: dict[str, dict[int, int]] = {}
    target: str | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if line == "end_of_record":
            target = None
        elif source := re.fullmatch(r"SF:(.*)", line, re.DOTALL):
            target = oracle_path(source[1], root, workspace)
            if target is not None:
                files.setdefault(target, {})
        elif (record := re.fullmatch(r"DA:([0-9]+),([0-9]+)(,.*)?", line, re.DOTALL)) and target is not None:
            number = int(record[1])
            if number:
                files[target][number] = files[target].get(number, 0) + int(record[2])
    return {path: lines for path, lines in files.items() if lines}


def oracle_diff(text: str) -> dict[str, set[int]]:
    """Read added lines from hunk headers with one pattern per header kind."""
    added: dict[str, set[int]] = {}
    target: set[int] | None = None
    for line in text.splitlines():
        if line[:4] == "+++ ":
            name = line[4:].strip()
            target = None if name == "/dev/null" else added.setdefault(name.removeprefix("b/"), set())
        elif target is not None and (hunk := re.match(r"@@ -[0-9]+(,[0-9]+)? \+([0-9]+)(,([0-9]+))? @@", line)):
            first, count = int(hunk[2]), int(hunk[4] if hunk[4] is not None else "1")
            if count <= 1_000_000:
                target.update(range(first, first + count))
    return {path: lines for path, lines in added.items() if lines}


def listing_entries(text: str) -> list[tuple[str, int]]:
    """Read a ``bundle`` corpus file: one ``<size>\\t<path>`` line per file."""
    entries = []
    for line in text.split("\n"):
        size, tab, path = line.partition("\t")
        if tab and re.fullmatch("-?[0-9]{1,12}", size):
            entries.append((path, int(size)))
    return entries


#: Characters a kept bundle path segment may hold.
BUNDLE_SEGMENT_CHARS = frozenset("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")


def oracle_bundle(entries: list[tuple[str, int]]) -> list[str]:
    """Keep bundle files by splitting paths into segments rather than one pattern."""

    def safe(path: str) -> bool:
        segments = path.split("/")
        if not 1 <= len(segments) <= 8:
            return False
        for segment in segments:
            if not 1 <= len(segment) <= 128 or segment[0] in ".-" or not set(segment) <= BUNDLE_SEGMENT_CHARS:
                return False
        name = segments[-1]
        return "." in name and name.rsplit(".", 1)[1].lower() in cg.BUNDLE_EXTENSIONS

    kept: list[str] = []
    total = 0
    for path, size in sorted(set(entries)):
        if safe(path) and 0 <= size <= cg.MAX_BUNDLE_FILE_BYTES:
            if len(kept) >= cg.MAX_BUNDLE_FILES or total + size > cg.MAX_BUNDLE_BYTES:
                break
            kept.append(path)
            total += size
    return kept


def assert_reads_like_oracle(case: unittest.TestCase, kind: str, text: str, label: str) -> None:
    """The module reads ``text`` as the oracle does, and keeps paths inside."""
    if kind == "lcov":
        got = cg.parse_lcov(text, CORPUS_ROOT, CORPUS_WORKSPACE)
        case.assertEqual(got, oracle_lcov(text, CORPUS_ROOT, CORPUS_WORKSPACE), label)
        for path, lines in got.items():
            parts = path.split("/")
            case.assertFalse(path.startswith("/") or "" in parts or "." in parts or ".." in parts, f"{label}: {path!r}")
            case.assertTrue(all(n > 0 and hits >= 0 for n, hits in lines.items()), label)
    elif kind == "junit":
        assert_junit_invariants(case, text, label)
    elif kind == "bundle":
        entries = listing_entries(text)
        kept = cg.select_bundle_files(entries)
        case.assertEqual(kept, oracle_bundle(entries), label)
        sizes = dict(entries)
        case.assertLessEqual(sum(sizes[path] for path in kept), cg.MAX_BUNDLE_BYTES, label)
        for path in kept:
            case.assertFalse(path.startswith("/") or ".." in path.split("/") or "\\" in path, f"{label}: {path!r}")
    else:
        case.assertEqual(cg.parse_diff(text), oracle_diff(text), label)


#: Characters no kept JUnit name contains.
NAME_FORBIDDEN = re.compile("[\x00-\x1f\x7f-\x9f\ufffe\uffff]")


def assert_junit_invariants(case: unittest.TestCase, text: str, label: str) -> None:
    """``parse_junit`` keeps its limits and reads back what it writes.

    The standard library has no second XML parser to serve as an oracle, so
    the oracle is the boundary's contract: a document type declaration
    reads as empty; every kept name is one line of safe characters within
    its limit; durations are finite and in range; failure text appears only
    on a failed or errored case; and the written report reads back to the
    same cases.
    """
    cases = cg.parse_junit(text)
    if cg.DTD_RE.search(text):
        case.assertEqual(cases, [], label)
    for test in cases:
        for value in (test.classname, test.name):
            case.assertLessEqual(len(value), cg.MAX_NAME_CHARS, label)
            case.assertIsNone(NAME_FORBIDDEN.search(value), f"{label}: {value!r}")
        case.assertTrue(0 <= test.seconds <= cg.MAX_TEST_SECONDS, label)
        case.assertLessEqual(len(test.failure), cg.MAX_FAILURE_CHARS, label)
        case.assertIsNone(cg.UNSAFE_CHARS_RE.search(test.failure), label)
        case.assertNotIn("\r", test.failure, label)
        if test.outcome not in (cg.Outcome.FAILED, cg.Outcome.ERROR):
            case.assertEqual(test.failure, "", label)
    written = cg.write_junit("area", cases)
    case.assertNotRegex(written, cg.DTD_RE, label)
    case.assertEqual(cg.parse_junit(written), cases, label)


def corpus() -> list[tuple[str, str, str]]:
    """Each corpus file as (kind, case, text)."""
    cases = []
    for path in sorted(CORPUS.iterdir()):
        kind, _, name = path.name.partition("__")
        cases.append((kind, name, path.read_bytes().decode("utf-8", "replace")))
    return cases


class ConfigTest(unittest.TestCase):
    """The areas file parses into areas and rejects bad shapes."""

    def test_parses_areas_in_order(self) -> None:
        config = cg.parse_config(CONFIG)
        self.assertTrue(config.enforce)
        self.assertEqual(config.patch_target, 80)
        self.assertEqual([a.name for a in config.areas], ["rust", "site"])
        self.assertEqual(config.areas[0].root, ".")
        self.assertEqual(config.areas[1].root, "tools/site")

    def test_rejects_unknown_mode(self) -> None:
        with self.assertRaises(cg.ConfigError):
            cg.parse_config(CONFIG.replace('"enforce"', '"strict"'))

    def test_rejects_missing_key(self) -> None:
        with self.assertRaises(cg.ConfigError):
            cg.parse_config(CONFIG.replace("floor = 50\n", ""))

    def test_rejects_invalid_toml(self) -> None:
        with self.assertRaises(cg.ConfigError):
            cg.parse_config("mode = ")

    def test_repository_areas_file_parses(self) -> None:
        text = (ROOT / cg.AREAS_FILE).read_text(encoding="utf-8")
        config = cg.parse_config(text)
        self.assertGreater(len(config.areas), 0)
        reports = [a.report for a in config.areas]
        self.assertEqual(len(reports), len(set(reports)), "report names must be unique")
        self.assertIn(tomllib.loads(text)["mode"], ("report", "enforce"))

    def test_owns_needs_prefix_and_suffix(self) -> None:
        rust = area()
        self.assertTrue(rust.owns("crates/a/src/lib.rs"))
        self.assertFalse(rust.owns("crates/a/Cargo.toml"))
        self.assertFalse(rust.owns("tools/x.rs"))


class NormalizeTest(unittest.TestCase):
    """LCOV paths become repository-relative or are dropped."""

    def test_relative_path_joins_root(self) -> None:
        self.assertEqual(cg.normalize("src/app.ts", "tools/demo", "/w"), "tools/demo/src/app.ts")

    def test_dot_root_and_dot_segments(self) -> None:
        self.assertEqual(cg.normalize("./crates/a/lib.rs", ".", "/w"), "crates/a/lib.rs")

    def test_absolute_inside_workspace(self) -> None:
        self.assertEqual(cg.normalize("/w/crates/a/lib.rs", ".", "/w"), "crates/a/lib.rs")

    def test_absolute_outside_workspace_is_dropped(self) -> None:
        self.assertIsNone(cg.normalize("/home/u/.cargo/registry/x.rs", ".", "/w"))

    def test_climbing_out_is_dropped(self) -> None:
        self.assertIsNone(cg.normalize("../../etc/passwd", "tools", "/w"))

    def test_windows_separators(self) -> None:
        self.assertEqual(cg.normalize("crates\\a\\lib.rs", ".", "/w"), "crates/a/lib.rs")


class LcovTest(unittest.TestCase):
    """LCOV records merge per file and tolerate malformed lines."""

    def test_merges_records_and_sums_hits(self) -> None:
        text = "\n".join(
            [
                "TN:",
                "SF:crates/a/lib.rs",
                "DA:1,0",
                "DA:2,3",
                "end_of_record",
                "SF:/w/crates/a/lib.rs",
                "DA:1,2",
                "DA:3,0",
                "end_of_record",
            ]
        )
        self.assertEqual(cg.parse_lcov(text, ".", "/w"), {"crates/a/lib.rs": {1: 2, 2: 3, 3: 0}})

    def test_skips_malformed_and_foreign_lines(self) -> None:
        text = "\n".join(
            [
                "DA:9,9",
                "SF:/elsewhere/dep.rs",
                "DA:1,1",
                "end_of_record",
                "SF:crates/b.rs",
                "DA:x,1",
                "DA:4",
                "DA:0,1",
                "DA:5,0",
                "DA:5,-2",
                "DA:6,1,checksum",
                "DA:7,1.0",
                "DA:8,1e400",
                "end_of_record",
                "SF:crates/empty.rs",
                "end_of_record",
            ]
        )
        self.assertEqual(cg.parse_lcov(text, ".", "/w"), {"crates/b.rs": {5: 0, 6: 1}})


class DiffTest(unittest.TestCase):
    """Added lines come from the new side of each hunk."""

    def test_reads_hunks(self) -> None:
        diff = "\n".join(
            [
                "diff --git a/crates/a.rs b/crates/a.rs",
                "--- a/crates/a.rs",
                "+++ b/crates/a.rs",
                "@@ -1,0 +2,3 @@",
                "+x",
                "@@ -10 +12 @@",
                "@@ -20,2 +22,0 @@",
                "diff --git a/old.rs b/old.rs",
                "--- a/old.rs",
                "+++ /dev/null",
                "@@ -1,4 +0,0 @@",
                "diff --git a/new.rs b/new.rs",
                "--- /dev/null",
                "+++ b/new.rs",
                "@@ -0,0 +1 @@",
            ]
        )
        self.assertEqual(cg.parse_diff(diff), {"crates/a.rs": {2, 3, 4, 12}, "new.rs": {1}})


class JudgeTest(unittest.TestCase):
    """Patch coverage counts executable changed lines in measured areas."""

    def config(self, floor: float = 0) -> object:
        return cg.Config(enforce=True, patch_target=80, areas=(area(floor=floor),))

    def test_patch_counts_only_executable_lines(self) -> None:
        result = cg.AreaResult(area(), True, {"crates/a.rs": {1: 1, 2: 0, 3: 1, 4: 1, 5: 1}})
        verdict = cg.judge(self.config(), [result], {"crates/a.rs": {1, 2, 3, 4, 5, 6, 7}})
        self.assertEqual(verdict.patch_percent, 80.0)
        self.assertEqual(verdict.files, (cg.FileOutcome("crates/a.rs", 4, (2,)),))
        self.assertEqual(verdict.failures, ())

    def test_below_target_fails(self) -> None:
        result = cg.AreaResult(area(), True, {"crates/a.rs": {1: 0, 2: 1}})
        verdict = cg.judge(self.config(), [result], {"crates/a.rs": {1, 2}})
        self.assertEqual(verdict.patch_percent, 50.0)
        self.assertEqual(len(verdict.failures), 1)
        self.assertIn("Patch coverage is 50.0%", verdict.failures[0])

    def test_unmeasured_area_is_listed_not_counted(self) -> None:
        result = cg.AreaResult(area(), False)
        verdict = cg.judge(self.config(), [result], {"crates/a.rs": {1}, "README.md": {1}})
        self.assertIsNone(verdict.patch_percent)
        self.assertEqual(verdict.unmeasured, ("crates/a.rs",))
        self.assertEqual(verdict.failures, ())

    def test_partial_run_skips_floor(self) -> None:
        result = cg.AreaResult(area(floor=75), True, {"crates/a.rs": {1: 1, 2: 0}}, partial=True)
        verdict = cg.judge(self.config(), [result], {"crates/a.rs": {2}})
        self.assertEqual(verdict.patch_percent, 0.0)
        self.assertEqual(len(verdict.failures), 1)
        self.assertIn("Patch coverage", verdict.failures[0])

    def test_floor(self) -> None:
        result = cg.AreaResult(area(floor=75), True, {"crates/a.rs": {1: 1, 2: 0}})
        verdict = cg.judge(self.config(), [result], {})
        self.assertEqual(verdict.failures, ("rust line coverage is 50.0%, below its 75% floor.",))


class RenderTest(unittest.TestCase):
    """The summary lists areas, missed lines and failures."""

    def test_ranges(self) -> None:
        self.assertEqual(cg.ranges((1, 2, 3, 7, 9, 10)), "1-3, 7, 9-10")
        self.assertEqual(cg.ranges(tuple(range(1, 50, 2))).count(","), cg.MAX_LISTED_LINES)
        self.assertTrue(cg.ranges(tuple(range(1, 50, 2))).endswith("…"))

    def test_render(self) -> None:
        config = cg.Config(enforce=False, patch_target=80, areas=(area(), area("site")))
        results = [
            cg.AreaResult(config.areas[0], True, {"crates/a.rs": {1: 1, 2: 0}}),
            cg.AreaResult(config.areas[1], False),
        ]
        verdict = cg.judge(config, results, {"crates/a.rs": {1, 2}})
        text = cg.render(config, verdict)
        self.assertIn("## Coverage (report only)", text)
        self.assertIn("**50.0%** of 2 changed executable lines", text)
        self.assertIn("| rust | 50.0% (1/2) | 0% |", text)
        self.assertIn("| site | not measured in this run | 0% |", text)
        self.assertIn("- `crates/a.rs`: 2", text)
        self.assertIn("below the 80% target", text)


class PlanTest(unittest.TestCase):
    """Only checks that ran the whole workspace count as full."""

    def test_reads_workspace_checks(self) -> None:
        plan = '{"execution": {"checks": {"rust_test": {"params": {"workspace": true}}, "fmt": {"params": {}}}}}'
        self.assertEqual(cg.full_workspace_checks(plan), frozenset({"rust_test"}))

    def test_selective_and_malformed_plans(self) -> None:
        self.assertEqual(cg.full_workspace_checks('{"execution": {"checks": {"rust_test": {"params": {"packages": ["a"]}}}}}'), frozenset())
        self.assertEqual(cg.full_workspace_checks("not json"), frozenset())
        self.assertEqual(cg.full_workspace_checks('{"execution": []}'), frozenset())

    def test_repository_rust_area_is_workspace_gated(self) -> None:
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        rust = next(a for a in config.areas if a.name == "rust")
        self.assertEqual(rust.workspace_check, "rust_test")


class CorpusTest(unittest.TestCase):
    """The seed corpus reads as the oracle reads it."""

    def test_corpus_kinds(self) -> None:
        cases = corpus()
        self.assertGreaterEqual(len(cases), 10)
        self.assertEqual({kind for kind, _, _ in cases}, {"lcov", "diff", "junit", "bundle"})

    def test_corpus_matches_oracle(self) -> None:
        for kind, name, text in corpus():
            with self.subTest(case=f"{kind}__{name}"):
                assert_reads_like_oracle(self, kind, text, name)

    def test_corpus_highlights(self) -> None:
        read = {f"{k}__{n}": t for k, n, t in corpus()}
        lcov = lambda name: cg.parse_lcov(read[name], CORPUS_ROOT, CORPUS_WORKSPACE)  # noqa: E731
        self.assertEqual(lcov("lcov__climbing"), {"tools/site/b.ts": {2: 1}, "tools": {1: 1}})
        self.assertEqual(set(lcov("lcov__absolute_outside")), set())
        self.assertEqual(set(lcov("lcov__odd_absolute")), {"a.ts", "c.ts", "d.ts"})
        self.assertEqual(lcov("lcov__malformed_da"), {"tools/site/a.ts": {6: 1, 7: 2, 99999999999999999999: 1}})
        self.assertEqual(cg.parse_diff(read["diff__huge_hunk"]), {"x.ts": {5, 6}})
        junit = lambda name: cg.parse_junit(read[name])  # noqa: E731
        self.assertEqual(junit("junit__doctype_entities"), [])
        self.assertEqual(junit("junit__malformed"), [])
        outcomes = [t.outcome for t in junit("junit__pytest_legacy")]
        self.assertEqual(outcomes, [cg.Outcome.PASSED, cg.Outcome.FAILED, cg.Outcome.SKIPPED, cg.Outcome.ERROR])
        nextest = junit("junit__nextest")
        self.assertIn("panicked at crates/bookclerk-naming/src/lib.rs", nextest[2].failure)
        self.assertNotIn("captured stdout", nextest[2].failure)
        # A retry that passed in the end is a pass.
        self.assertEqual(nextest[3].outcome, cg.Outcome.PASSED)
        odd = junit("junit__odd_values")
        self.assertEqual([t.seconds for t in odd], [0.0, 0.0, cg.MAX_TEST_SECONDS, 0.0, 0.1, 0.0])
        self.assertEqual(odd[0].classname, "a b c")
        self.assertEqual(odd[4].failure, "m\nline\nbody\ntext")
        nested = junit("junit__nested_suites")
        self.assertEqual([(t.name, t.outcome) for t in nested], [("deep", cg.Outcome.SKIPPED), ("both", cg.Outcome.ERROR)])
        bundle = lambda name: cg.select_bundle_files(listing_entries(read[name]))  # noqa: E731
        self.assertEqual(
            bundle("bundle__vite_ui"),
            ["assets/index-BkQ2x9.js", "assets/index-Cz81.css", "assets/inter-latin.woff2", "favicon.svg", "index.html"],
        )
        self.assertEqual(bundle("bundle__wrangler"), ["worker.js"])
        self.assertEqual(bundle("bundle__hostile"), ["a/b/c/d/e/f/g/h.js", "at-limit.js", "dup.js", "ok.JS"])


#: Fragments the mutator splices in: record keys, path tricks, digits.
FRAGMENTS = (
    "SF:",
    "DA:",
    "end_of_record",
    "\n",
    "\r\n",
    ",",
    "../",
    "/w/",
    "//",
    "\\",
    "./",
    "٣",
    "-1",
    "1e400",
    "99999999999",
    "+++ b/",
    "+++ /dev/null",
    "@@ -1 +2,3 @@",
    "@@ -0,0 +1 @@",
    " ",
    "<testcase ",
    "</testcase>",
    "<failure>",
    "<error message='",
    "<skipped/>",
    ' time="',
    "<!DOCTYPE x>",
    "&amp;",
    "&#x85;",
    "&#13;",
    '"',
    "<",
    ">",
    "\t",
    ".js",
    ".map",
    "/.",
    "assets/",
)


class MutationFuzzTest(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 4000

    def mutate(self, rng: random.Random, text: str) -> str:
        for _ in range(rng.randint(1, 5)):
            pos = rng.randint(0, len(text))
            op = rng.random()
            if op < 0.5:
                text = text[:pos] + rng.choice(FRAGMENTS) + text[pos:]
            elif op < 0.75:
                text = text[:pos] + text[pos + rng.randint(1, 4) :]
            elif op < 0.9:
                lines = text.splitlines(keepends=True)
                if lines:
                    text = "".join(lines + [rng.choice(lines)])
            else:
                text = text[:pos] + chr(rng.randint(0, 0x2FF)) + text[pos:]
        return text

    def test_mutations_match_oracle(self) -> None:
        seeds = corpus()
        rng = random.Random(20261028)
        for n in range(self.ITERATIONS):
            kind, _, text = rng.choice(seeds)
            mutated = self.mutate(rng, text)
            # Any parser may be fed another's input.
            kind = kind if rng.random() < 0.9 else rng.choice(("lcov", "diff", "junit", "bundle"))
            assert_reads_like_oracle(self, kind, mutated, f"iteration {n}: {kind} {mutated!r}")

    def test_generated_paths_match_oracle(self) -> None:
        rng = random.Random(528)
        pieces = ["w", "tools", "site", "..", ".", "", "a.ts", "wx", "\\"]
        for n in range(self.ITERATIONS):
            path = rng.choice(["", "/", "//"]) + "/".join(rng.choice(pieces) for _ in range(rng.randint(1, 6)))
            self.assertEqual(
                cg.normalize(path, CORPUS_ROOT, CORPUS_WORKSPACE),
                oracle_path(path, CORPUS_ROOT, CORPUS_WORKSPACE),
                f"generated {n}: {path!r}",
            )


class MainTest(unittest.TestCase):
    """The script reads the reports directory and diff file end to end."""

    def run_main(self, reports: dict[str, str], diff: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            for name, text in reports.items():
                (base / "reports" / name).parent.mkdir(parents=True, exist_ok=True)
                (base / "reports" / name).write_text(text, encoding="utf-8")
            (base / "patch.diff").write_text(diff, encoding="utf-8")
            summary = base / "summary.md"
            cwd = Path.cwd()
            os.chdir(ROOT)
            try:
                status = cg.main(["--reports", str(base / "reports"), "--diff", str(base / "patch.diff"), "--summary", str(summary)])
            finally:
                os.chdir(cwd)
            return status, summary.read_text(encoding="utf-8")

    def test_report_mode_never_fails(self) -> None:
        lcov = "SF:index.ts\nDA:1,0\nDA:2,0\nend_of_record\n"
        diff = "+++ b/tools/site/index.ts\n@@ -0,0 +1,2 @@\n"
        status, text = self.run_main({"site/lcov.info": lcov}, diff)
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        self.assertEqual(status, 1 if config.enforce else 0)
        self.assertIn("**0.0%** of 2 changed executable lines", text)
        self.assertIn("| rust | not measured in this run |", text)

    def test_missing_diff_only_warns_in_report_mode(self) -> None:
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as tmp:
            cwd = Path.cwd()
            os.chdir(ROOT)
            try:
                status = cg.main(["--reports", tmp, "--diff", str(Path(tmp) / "absent.diff"), "--summary", str(Path(tmp) / "s.md")])
            finally:
                os.chdir(cwd)
        self.assertEqual(status, 1 if config.enforce else 0)



class ExportTest(unittest.TestCase):
    """The upload's reports are the parsed ones, repository-relative."""

    def test_write_lcov_round_trips(self) -> None:
        hits = {"tools/site/b.ts": {3: 0, 1: 2}, "crates/a.rs": {7: 1}}
        text = cg.write_lcov(hits)
        self.assertEqual(text.splitlines()[0], "SF:crates/a.rs")
        self.assertEqual(cg.parse_lcov(text, ".", "/w"), hits)

    def test_mutated_reports_round_trip(self) -> None:
        rng = random.Random(41)
        seeds = [t for k, _, t in corpus() if k == "lcov"]
        for n in range(500):
            text = MutationFuzzTest().mutate(rng, rng.choice(seeds))
            hits = cg.parse_lcov(text, CORPUS_ROOT, CORPUS_WORKSPACE)
            self.assertEqual(cg.parse_lcov(cg.write_lcov(hits), ".", CORPUS_WORKSPACE), hits, f"iteration {n}: {text!r}")

    def test_export_writes_measured_areas(self) -> None:
        config = cg.parse_config(CONFIG)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            (base / "in" / "site").mkdir(parents=True)
            (base / "in" / "site" / "lcov.info").write_text("SF:a.ts\nDA:1,1\nend_of_record\nSF:../x.rs\nDA:1,1\n", encoding="utf-8")
            cwd = Path.cwd()
            os.chdir(base)
            try:
                written = cg.export(config, base / "in", base / "out", frozenset())
            finally:
                os.chdir(cwd)
            self.assertEqual(written, ["site"])
            self.assertEqual((base / "out" / "site.lcov").read_text(encoding="utf-8"), "SF:tools/site/a.ts\nDA:1,1\nend_of_record\n")
            self.assertFalse((base / "out" / "rust.lcov").exists())

    def test_junit_limits(self) -> None:
        long_name = "n" * (cg.MAX_NAME_CHARS + 50)
        failure = "f" * (cg.MAX_FAILURE_CHARS + 50)
        text = f'<testsuites><testcase classname="c" name="{long_name}"><failure>{failure}</failure></testcase></testsuites>'
        (test,) = cg.parse_junit(text)
        self.assertEqual(len(test.name), cg.MAX_NAME_CHARS)
        self.assertEqual(len(test.failure), cg.MAX_FAILURE_CHARS)
        many = "<testsuites>" + '<testcase name="t"/>' * 5 + "</testsuites>"
        with mock.patch.object(cg, "MAX_TEST_CASES", 3):
            self.assertEqual(len(cg.parse_junit(many)), 3)
        with mock.patch.object(cg, "MAX_JUNIT_CHARS", len(many) - 1):
            self.assertEqual(cg.parse_junit(many), [])

    def test_export_writes_test_results(self) -> None:
        config = cg.parse_config(CONFIG.replace('report = "site/lcov.info"\n', 'report = "site/lcov.info"\ntest_results = "site/junit.xml"\n'))
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            (base / "in" / "site").mkdir(parents=True)
            junit = (CORPUS / "junit__pytest_legacy").read_text(encoding="utf-8")
            (base / "in" / "site" / "junit.xml").write_text(junit, encoding="utf-8")
            written = cg.export(config, base / "in", base / "out", frozenset())
            self.assertEqual(written, ["site test results"])
            self.assertEqual(cg.parse_junit((base / "out" / "site.junit.xml").read_text(encoding="utf-8")), cg.parse_junit(junit))

    def test_bundle_limits(self) -> None:
        many = [(f"f{n:05}.js", 1) for n in range(cg.MAX_BUNDLE_FILES + 5)]
        self.assertEqual(len(cg.select_bundle_files(many)), cg.MAX_BUNDLE_FILES)
        big = [(f"f{n}.js", cg.MAX_BUNDLE_FILE_BYTES) for n in range(6)]
        self.assertEqual(cg.select_bundle_files(big), ["f0.js", "f1.js", "f2.js", "f3.js"])

    def test_export_bundles_skips_links_and_unknown_bundles(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            secret = base / "secret.js"
            secret.write_text("secret", encoding="utf-8")
            ui = base / "in" / "ui"
            (ui / "assets").mkdir(parents=True)
            (ui / "assets" / "index.js").write_text("app", encoding="utf-8")
            (ui / "assets" / "index.js.map").write_text("map", encoding="utf-8")
            (ui / "linked.js").symlink_to(secret)
            (ui / "hard.js").hardlink_to(secret)
            (ui / "linked-dir").symlink_to(base, target_is_directory=True)
            (base / "in" / "other").mkdir()
            (base / "in" / "other" / "x.js").write_text("x", encoding="utf-8")
            (base / "in" / "site").symlink_to(ui, target_is_directory=True)
            written = cg.export_bundles(base / "in", base / "out")
            self.assertEqual(written, ["ui"])
            files = sorted(p.relative_to(base / "out").as_posix() for p in (base / "out").rglob("*") if p.is_file())
            self.assertEqual(files, ["ui/assets/index.js"])
            self.assertEqual((base / "out" / "ui" / "assets" / "index.js").read_text(encoding="utf-8"), "app")

    def test_generated_files_are_left_out(self) -> None:
        config = cg.parse_config(CONFIG.replace("patch_target = 80\n", 'patch_target = 80\ngenerated = ["tools/site/gen.ts", "crates/a/src/gen.rs"]\n', 1))
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            (base / "in" / "site").mkdir(parents=True)
            report = "SF:a.ts\nDA:1,1\nend_of_record\nSF:gen.ts\nDA:1,0\nend_of_record\n"
            (base / "in" / "site" / "lcov.info").write_text(report, encoding="utf-8")
            cwd = Path.cwd()
            os.chdir(base)
            try:
                results = cg.read_reports(config, base / "in", frozenset())
            finally:
                os.chdir(cwd)
        site = next(r for r in results if r.area.name == "site")
        self.assertEqual(set(site.hits), {"tools/site/a.ts"})
        # The rust area has no report: its generated file is not listed as unmeasured.
        verdict = cg.judge(config, results, {"tools/site/gen.ts": {1}, "crates/a/src/gen.rs": {1}})
        self.assertIsNone(verdict.patch_percent)
        self.assertEqual(verdict.unmeasured, ())

    def test_export_keeps_a_selective_run_off_the_area_flag(self) -> None:
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            (base / "in" / "rust").mkdir(parents=True)
            (base / "in" / "rust" / "lcov.info").write_text("SF:crates/a/src/lib.rs\nDA:1,1\nend_of_record\n", encoding="utf-8")
            cwd = Path.cwd()
            os.chdir(base)
            try:
                selective = cg.export(config, base / "in", base / "partial", frozenset())
                full = cg.export(config, base / "in", base / "full", frozenset({"rust_test"}))
            finally:
                os.chdir(cwd)
            self.assertEqual(selective, ["rust-partial"])
            self.assertEqual(full, ["rust"])



class UploadWorkflowTest(unittest.TestCase):
    """The Codecov upload stays unprivileged and matches the areas file."""

    @classmethod
    def setUpClass(cls) -> None:
        import yaml

        cls.areas = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8")).areas
        cls.workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "coverage-upload.yml").read_text(encoding="utf-8"))
        cls.codecov = yaml.safe_load((ROOT / ".github" / "codecov.yml").read_text(encoding="utf-8"))

    def upload_names(self) -> list[str]:
        """Name every file the export can write, in upload order."""
        names = []
        for area in self.areas:
            names.append(area.name)
            if area.workspace_check is not None:
                names.append(f"{area.name}-partial")
        return names

    def test_flags_match_areas(self) -> None:
        individual = self.codecov["flag_management"]["individual_flags"]
        flags = {f["name"]: f["paths"] for f in individual}
        expected = {a.name: list(a.paths) for a in self.areas}
        expected |= {f"{a.name}-partial": list(a.paths) for a in self.areas if a.workspace_check is not None}
        self.assertEqual(flags, expected)
        self.assertTrue(self.codecov["flag_management"]["default_rules"]["carryforward"])
        # A partial report counts only on the commit it measured.
        for flag in individual:
            self.assertEqual(flag.get("carryforward", True), not flag["name"].endswith("-partial"), flag["name"])

    def test_generated_files_match_the_gate(self) -> None:
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        self.assertEqual(sorted(self.codecov["ignore"]), sorted(config.generated))
        marker = re.compile(r"GENERATED FILE|do not edit this file by hand", re.IGNORECASE)
        for path in config.generated:
            head = (ROOT / path).read_text(encoding="utf-8").splitlines()[:6]
            self.assertTrue(any(marker.search(line) for line in head), f"{path} has no generated-file header")
        tracked = subprocess.run(["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.splitlines()
        for path in tracked:
            if not any(a.owns(path) for a in config.areas) or path in config.generated:
                continue
            head = (ROOT / path).read_text(encoding="utf-8", errors="replace").splitlines()[:6]
            self.assertFalse(any(marker.search(line) for line in head), f"{path} is generated: list it in `generated`")

    def test_components_cover_known_paths(self) -> None:
        for component in self.codecov["component_management"]["individual_components"]:
            for path in component["paths"]:
                self.assertTrue((ROOT / path).is_dir(), f"{component['component_id']}: {path}")

    def test_patch_target_matches_the_gate(self) -> None:
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        self.assertEqual(self.codecov["coverage"]["status"]["patch"]["default"]["target"], f"{config.patch_target:g}%")

    def test_one_upload_per_area_from_the_export(self) -> None:
        steps = self.workflow["jobs"]["upload"]["steps"]
        actions = [s for s in steps if str(s.get("uses", "")).startswith("codecov/codecov-action@")]
        uploads = [s for s in actions if "report_type" not in s["with"]]
        self.assertEqual([u["with"]["flags"] for u in uploads], self.upload_names())
        for upload in uploads:
            area = upload["with"]["flags"]
            self.assertEqual(upload["with"]["files"], f"coverage-upload/{area}.lcov")
            self.assertEqual(upload["if"], f"steps.target.outputs.upload == 'true' && hashFiles('coverage-upload/{area}.lcov') != ''")
            # Never empty: Codecov would take the default branch instead.
            self.assertEqual(upload["with"]["override_branch"], "${{ steps.target.outputs.branch }}")
            self.assertIs(upload["with"]["disable_search"], True)
            # The checkout is not the measured commit (see the workflow).
            self.assertIs(upload["with"]["disable_file_fixes"], True)
            self.assertFalse((ROOT / upload["with"]["network_filter"]).exists())
            self.assertIs(upload["with"]["use_oidc"], True)
            self.assertRegex(upload["uses"], r"@[0-9a-f]{40}$")
            self.assertEqual(upload["with"]["override_commit"], "${{ github.event.workflow_run.head_sha }}")
        export = next(s for s in steps if "--export coverage-upload" in s.get("run", ""))
        # Downloaded by name, the plan lands directly under the step's path.
        plan = next(s for s in steps if s.get("name") == "Download the CI plan")
        self.assertEqual(plan["with"]["name"], "ci-plan")
        self.assertNotIn("pattern", plan["with"])
        self.assertEqual(plan["with"]["path"], "${{ runner.temp }}/plan")
        self.assertIn('--plan "$RUNNER_TEMP/plan/ci-plan.json"', export["run"])
        self.assertLess(steps.index(export), steps.index(uploads[0]))

    def test_one_test_results_upload_per_area(self) -> None:
        steps = self.workflow["jobs"]["upload"]["steps"]
        results = [s for s in steps if s.get("with", {}).get("report_type") == "test_results"]
        self.assertEqual([r["with"]["flags"] for r in results], [a.name for a in self.areas if a.test_results])
        for upload in results:
            area = upload["with"]["flags"]
            self.assertEqual(upload["with"]["files"], f"coverage-upload/{area}.junit.xml")
            self.assertEqual(upload["if"], f"steps.target.outputs.upload == 'true' && hashFiles('coverage-upload/{area}.junit.xml') != ''")
            self.assertEqual(upload["with"]["override_branch"], "${{ steps.target.outputs.branch }}")
            self.assertEqual(upload["with"]["override_commit"], "${{ github.event.workflow_run.head_sha }}")
            self.assertIs(upload["with"]["disable_search"], True)
            self.assertIs(upload["with"]["use_oidc"], True)
            self.assertRegex(upload["uses"], r"@[0-9a-f]{40}$")

    def test_bundles_upload_only_what_the_export_kept(self) -> None:
        steps = {s.get("name"): s for s in self.workflow["jobs"]["upload"]["steps"]}
        download = steps["Download the bundles"]["with"]
        self.assertEqual(download["pattern"], "bundle-*")
        self.assertEqual(download["path"], "${{ runner.temp }}/bundles")
        self.assertIn('--bundles "$RUNNER_TEMP/bundles"', steps["Export the reports"]["run"])
        upload = steps["Upload the bundle sizes"]
        self.assertIn("steps.target.outputs.upload == 'true'", upload["if"])
        run = upload["run"]
        self.assertIn("npm ci --ignore-scripts", run)
        self.assertIn(f"for bundle in {' '.join(cg.BUNDLES)}; do", run)
        self.assertIn('"coverage-upload/bundles/$bundle"', run)
        self.assertNotIn("$RUNNER_TEMP/bundles", run)
        # Event values reach the script through env only.
        self.assertNotIn("${{", run)
        self.assertEqual(upload["env"]["BRANCH"], "${{ steps.target.outputs.branch }}")
        self.assertEqual(self.codecov["bundle_analysis"]["status"], "informational")

    def test_runs_unprivileged_from_the_default_branch(self) -> None:
        triggers = self.workflow.get("on", self.workflow.get(True))
        self.assertEqual(triggers, {"workflow_run": {"workflows": ["CI"], "types": ["completed"]}})
        self.assertEqual(self.workflow["permissions"], {})
        job = self.workflow["jobs"]["upload"]
        self.assertEqual(job["permissions"], {"actions": "read", "id-token": "write"})
        checkout = job["steps"][0]
        self.assertTrue(checkout["uses"].startswith("actions/checkout@"))
        # The default branch only, without a token left in .git/config.
        self.assertEqual(checkout["with"], {"persist-credentials": False})
        for step in job["steps"]:
            # Event text reaches shell steps through env only.
            self.assertNotIn("${{", step.get("run", ""), step.get("name"))
        self.assertIn("'cancelled'", job["if"])
        self.assertIn('["pull_request", "merge_group"]', job["if"])


if __name__ == "__main__":
    os.environ.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    unittest.main()
