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
import sys
import tempfile
import unittest
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


def assert_reads_like_oracle(case: unittest.TestCase, kind: str, text: str, label: str) -> None:
    """The module reads ``text`` as the oracle does, and keeps paths inside."""
    if kind == "lcov":
        got = cg.parse_lcov(text, CORPUS_ROOT, CORPUS_WORKSPACE)
        case.assertEqual(got, oracle_lcov(text, CORPUS_ROOT, CORPUS_WORKSPACE), label)
        for path, lines in got.items():
            parts = path.split("/")
            case.assertFalse(path.startswith("/") or "" in parts or "." in parts or ".." in parts, f"{label}: {path!r}")
            case.assertTrue(all(n > 0 and hits >= 0 for n, hits in lines.items()), label)
    else:
        case.assertEqual(cg.parse_diff(text), oracle_diff(text), label)


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
        self.assertEqual({kind for kind, _, _ in cases}, {"lcov", "diff"})

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
            # Either parser may be fed the other's input.
            kind = kind if rng.random() < 0.9 else ("diff" if kind == "lcov" else "lcov")
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
                written = cg.export(config, base / "in", base / "out")
            finally:
                os.chdir(cwd)
            self.assertEqual(written, ["site"])
            self.assertEqual((base / "out" / "site.lcov").read_text(encoding="utf-8"), "SF:tools/site/a.ts\nDA:1,1\nend_of_record\n")
            self.assertFalse((base / "out" / "rust.lcov").exists())



class UploadWorkflowTest(unittest.TestCase):
    """The Codecov upload stays unprivileged and matches the areas file."""

    @classmethod
    def setUpClass(cls) -> None:
        import yaml

        cls.areas = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8")).areas
        cls.workflow = yaml.safe_load((ROOT / ".github" / "workflows" / "coverage-upload.yml").read_text(encoding="utf-8"))
        cls.codecov = yaml.safe_load((ROOT / ".github" / "codecov.yml").read_text(encoding="utf-8"))

    def test_flags_match_areas(self) -> None:
        flags = {f["name"]: f["paths"] for f in self.codecov["flag_management"]["individual_flags"]}
        self.assertEqual(flags, {a.name: list(a.paths) for a in self.areas})
        self.assertTrue(self.codecov["flag_management"]["default_rules"]["carryforward"])

    def test_patch_target_matches_the_gate(self) -> None:
        config = cg.parse_config((ROOT / cg.AREAS_FILE).read_text(encoding="utf-8"))
        self.assertEqual(self.codecov["coverage"]["status"]["patch"]["default"]["target"], f"{config.patch_target:g}%")

    def test_one_upload_per_area_from_the_export(self) -> None:
        steps = self.workflow["jobs"]["upload"]["steps"]
        uploads = [s for s in steps if str(s.get("uses", "")).startswith("codecov/codecov-action@")]
        self.assertEqual([u["with"]["flags"] for u in uploads], [a.name for a in self.areas])
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
        self.assertLess(steps.index(export), steps.index(uploads[0]))

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
