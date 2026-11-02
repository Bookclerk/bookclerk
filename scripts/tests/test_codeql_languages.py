#!/usr/bin/env python3
"""CodeQL language chooser: seed corpus replay, mutation fuzzing and wiring.

The changed paths ``scripts/codeql_languages.py`` reads are file names the
pull request author chose, so the reader is an untrusted-input boundary
(docs/contributing/fuzzing.md). Every corpus input and every mutation of one
must give the same answer as an independent oracle: Rust is analyzed exactly
when some NUL-separated name is not UTF-8 or names a Rust input.
"""

from __future__ import annotations

import importlib.util
import json
import os
import random
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

import yaml

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "codeql_languages_corpus"
WORKFLOW = ROOT / ".github" / "workflows" / "codeql.yml"


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("codeql_languages", ROOT / "scripts" / "codeql_languages.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["codeql_languages"] = module
    spec.loader.exec_module(module)
    return module


cl = _load()


def _load_reuse() -> ModuleType:
    spec = importlib.util.spec_from_file_location("codeql_reuse", ROOT / "scripts" / "codeql_reuse.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

# The oracle's own reading of a Rust input, written without fnmatch.
ORACLE_NAMES = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"}
ORACLE_SUFFIXES = (".rs", ".capnp", ".proto")
ORACLE_EXACT = {".github/workflows/codeql.yml", "scripts/codeql_languages.py", "scripts/codeql_reuse.py"}


def oracle_needs_rust(raw: bytes) -> bool:
    """Independent answer: does this diff listing need the Rust analysis?"""
    for chunk in raw.split(b"\x00"):
        if chunk == b"":
            continue
        try:
            path = chunk.decode("utf-8")
        except UnicodeDecodeError:
            return True
        base = path[path.rfind("/") + 1 :]
        if base in ORACLE_NAMES or base.endswith(ORACLE_SUFFIXES):
            return True
        if path.startswith(".cargo/") or "/.cargo/" in path or path in ORACLE_EXACT:
            return True
    return False


def analyzes_rust(raw: bytes) -> bool:
    """Whether the chooser classifies this listing as changing a Rust input."""
    return cl.rust_reason(cl.parse_paths(raw)) is not None


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus (files named ``rust__<case>`` or ``norust__<case>``)."""

    def test_corpus_matches_names_and_oracle(self) -> None:
        seeds = sorted(CORPUS.iterdir())
        self.assertGreaterEqual(len(seeds), 10)
        for seed in seeds:
            with self.subTest(seed=seed.name):
                expect, _, case = seed.name.partition("__")
                self.assertIn(expect, ("rust", "norust"))
                self.assertTrue(case)
                raw = seed.read_bytes()
                self.assertEqual(analyzes_rust(raw), expect == "rust")
                self.assertEqual(analyzes_rust(raw), oracle_needs_rust(raw))

    def test_other_events_analyze_everything(self) -> None:
        for event in ("push", "schedule", "workflow_dispatch", "merge_group", "something_new"):
            with self.subTest(event=event):
                choice = cl.choose(event, [])
                self.assertEqual(choice.languages, (*cl.ALWAYS, cl.RUST))

    def test_build_script_inputs_are_rust_inputs(self) -> None:
        # CodeQL's `none` mode runs build scripts, so a file a build script
        # reads (a schema it compiles into Rust) changes the Rust analysis.
        # Every existing file a tracked build.rs names by path must count.
        listed = subprocess.run(
            ["git", "ls-files", "-z", "--", "*build.rs"], cwd=ROOT, capture_output=True, check=True
        ).stdout
        scripts = [p for p in listed.decode("utf-8").split("\0") if p.endswith("build.rs")]
        self.assertTrue(scripts)
        named = 0
        for script in scripts:
            crate = (ROOT / script).parent
            for literal in re.findall(r'"([^"\s{}]+\.[A-Za-z0-9]+)"', (ROOT / script).read_text(encoding="utf-8")):
                target = crate / literal
                if not target.is_file():
                    continue
                rel = target.resolve().relative_to(ROOT).as_posix()
                named += 1
                with self.subTest(script=script, path=rel):
                    self.assertTrue(cl.is_rust_input(rel))
        # The plugin ABI's Cap'n Proto schemas and audible-rs's protobuf.
        self.assertGreaterEqual(named, 3)

    def test_matrix_is_build_mode_none_for_every_language(self) -> None:
        doc = json.loads(cl.matrix_json(cl.choose("pull_request", ["docs/a.md"])))
        self.assertEqual([e["language"] for e in doc["include"]], [*cl.ALWAYS, cl.RUST])
        self.assertTrue(all(e["build-mode"] == "none" for e in doc["include"]))


class FuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ALPHABET = b"\x00/._-aArsCcargolktmpny\n\"\xff\xc3"

    def mutate(self, rng: random.Random, raw: bytes) -> bytes:
        data = bytearray(raw)
        for _ in range(rng.randint(1, 6)):
            op = rng.random()
            pos = rng.randint(0, len(data))
            if op < 0.4:
                data[pos:pos] = bytes([rng.choice(self.ALPHABET)])
            elif op < 0.6 and data:
                del data[min(pos, len(data) - 1)]
            elif op < 0.8:
                data[pos:pos] = rng.choice([b".rs", b"\x00", b"Cargo.toml", b".cargo/", b"/.cargo/", b".capnp", b"/"])
            elif data:
                data[min(pos, len(data) - 1)] = rng.randrange(256)
        return bytes(data)

    def test_mutations_match_oracle(self) -> None:
        rng = random.Random(20261102)
        seeds = [p.read_bytes() for p in sorted(CORPUS.iterdir())]
        for i in range(4000):
            raw = self.mutate(rng, rng.choice(seeds))
            self.assertEqual(analyzes_rust(raw), oracle_needs_rust(raw), f"iteration {i}: {raw!r}")

    def test_random_listings_match_oracle(self) -> None:
        rng = random.Random(7)
        for i in range(2000):
            raw = bytes(rng.randrange(256) for _ in range(rng.randint(0, 40)))
            self.assertEqual(analyzes_rust(raw), oracle_needs_rust(raw), f"iteration {i}: {raw!r}")


class CliTests(unittest.TestCase):
    """The command line writes the matrix the workflow reads."""

    def run_main(self, event: str, raw: bytes | None) -> tuple[dict, str]:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            summary = Path(tmp) / "summary"
            args = ["--event", event]
            if raw is not None:
                paths = Path(tmp) / "paths"
                paths.write_bytes(raw)
                args += ["--paths-file", str(paths)]
            old = {k: os.environ.get(k) for k in ("GITHUB_OUTPUT", "GITHUB_STEP_SUMMARY")}
            os.environ["GITHUB_OUTPUT"] = str(out)
            os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
            try:
                self.assertEqual(cl.main(args), 0)
            finally:
                for k, v in old.items():
                    if v is None:
                        os.environ.pop(k, None)
                    else:
                        os.environ[k] = v
            lines = out.read_text(encoding="utf-8").splitlines(keepends=True)
            self.assertEqual(len(lines), 2)
            self.assertTrue(lines[0].startswith("matrix=") and lines[0].endswith("\n"))
            self.assertIn(lines[1], ("reuse-rust=true\n", "reuse-rust=false\n"))
            self.assertIn("CodeQL languages:", summary.read_text(encoding="utf-8"))
            return json.loads(lines[0][len("matrix=") :]), lines[1].strip().split("=", 1)[1]

    def test_pull_request_without_rust_reuses_main(self) -> None:
        # The merge rule blocks a pull request missing `/language:rust`, so
        # Rust stays in the matrix; the job reuses main's results instead.
        self.assertIs(cl.PULL_REQUESTS_REUSE_RUST, True)
        matrix, reuse = self.run_main("pull_request", b"docs/a.md\x00")
        self.assertEqual([e["language"] for e in matrix["include"]], [*cl.ALWAYS, cl.RUST])
        self.assertEqual(reuse, "true")

    def test_reuse_can_be_turned_off(self) -> None:
        old = cl.PULL_REQUESTS_REUSE_RUST
        cl.PULL_REQUESTS_REUSE_RUST = False
        try:
            choice = cl.choose("pull_request", ["docs/a.md"])
            self.assertEqual(choice.languages, (*cl.ALWAYS, cl.RUST))
            self.assertIs(choice.reuse_rust, False)
        finally:
            cl.PULL_REQUESTS_REUSE_RUST = old

    def test_pull_request_with_rust_analyzes_it(self) -> None:
        matrix, reuse = self.run_main("pull_request", b"crates/a/src/lib.rs\x00")
        self.assertIn("rust", [e["language"] for e in matrix["include"]])
        self.assertEqual(reuse, "false")

    def test_unreadable_name_analyzes_rust(self) -> None:
        self.assertIs(cl.choose("pull_request", ["docs/a.md", None]).reuse_rust, False)

    def test_other_events_never_reuse(self) -> None:
        for event in ("push", "schedule", "workflow_dispatch", "merge_group", "something_new"):
            with self.subTest(event=event):
                self.assertIs(cl.choose(event, ["docs/a.md"]).reuse_rust, False)

    def test_matrix_is_the_same_whether_or_not_rust_is_reused(self) -> None:
        # Code scanning records the matrix values with each analysis; a
        # pull request's must equal main's to match its configuration.
        self.assertEqual(
            cl.matrix_json(cl.choose("pull_request", ["docs/a.md"])), cl.matrix_json(cl.choose("push", []))
        )

    def test_push_needs_no_paths(self) -> None:
        matrix, reuse = self.run_main("push", None)
        self.assertIn("rust", [e["language"] for e in matrix["include"]])
        self.assertEqual(reuse, "false")

    def test_unreadable_paths_file_fails(self) -> None:
        self.assertEqual(cl.main(["--event", "pull_request", "--paths-file", "/nonexistent/paths"]), 2)


class WorkflowTests(unittest.TestCase):
    """codeql.yml uses the chooser and analyzes what default setup did."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.wf = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        cls.on = cls.wf.get("on") or cls.wf.get(True)

    def test_triggers(self) -> None:
        self.assertEqual(set(self.on), {"push", "pull_request", "schedule", "workflow_dispatch"})
        self.assertEqual(self.on["push"]["branches"], ["main"])
        # No path filter: every pull request gets the always-on languages.
        self.assertNotIn("paths", self.on["pull_request"])
        self.assertNotIn("paths-ignore", self.on["pull_request"])

    def test_diff_is_nul_separated_without_renames(self) -> None:
        steps = self.wf["jobs"]["languages"]["steps"]
        diff = next(s for s in steps if s.get("name") == "Diff the pull request")
        self.assertIn("git diff -z --no-renames --name-only HEAD^1 HEAD", diff["run"])
        checkout = steps[0]
        self.assertEqual(checkout["with"]["fetch-depth"], 2)
        choose = next(s for s in steps if s.get("id") == "choose")
        self.assertIn("scripts/codeql_languages.py", choose["run"])
        self.assertNotIn("${{", choose["run"])

    def test_analyze_reads_the_matrix_and_uploads_per_language(self) -> None:
        job = self.wf["jobs"]["analyze"]
        self.assertEqual(job["needs"], "languages")
        self.assertEqual(job["strategy"]["matrix"], "${{ fromJSON(needs.languages.outputs.matrix) }}")
        self.assertIs(job["strategy"]["fail-fast"], False)
        self.assertEqual(job["permissions"]["security-events"], "write")
        steps = job["steps"]
        init = next(s for s in steps if str(s.get("uses", "")).startswith("github/codeql-action/init@"))
        analyze = next(s for s in steps if str(s.get("uses", "")).startswith("github/codeql-action/analyze@"))
        self.assertEqual(init["with"]["languages"], "${{ matrix.language }}")
        self.assertEqual(init["with"]["build-mode"], "${{ matrix.build-mode }}")
        self.assertNotIn("queries", init["with"])
        self.assertEqual(analyze["with"]["category"], "/language:${{ matrix.language }}")
        self.assertEqual(init["uses"].split("@")[1], analyze["uses"].split("@")[1])

    def test_only_main_saves_the_extractor_cache(self) -> None:
        steps = self.wf["jobs"]["analyze"]["steps"]
        saves = [s for s in steps if str(s.get("uses", "")).startswith("actions/cache/save@")]
        self.assertEqual(len(saves), 1)
        self.assertIn("github.ref == 'refs/heads/main'", saves[0]["if"])
        self.assertFalse([s for s in steps if str(s.get("uses", "")).startswith("actions/cache@")])

    def test_job_env_reads_only_contexts_github_allows_there(self) -> None:
        # GitHub rejects the whole workflow when a job-level `env` reads the
        # `runner` context, so every run fails before any job starts.
        for name, job in self.wf["jobs"].items():
            for key, value in (job.get("env") or {}).items():
                self.assertNotIn("runner.", str(value), f"{name}: {key}")
        steps = self.wf["jobs"]["analyze"]["steps"]
        sets_dir = [s for s in steps if "CODEQL_EXTRACTOR_RUST_OPTION_CARGO_TARGET_DIR=" in str(s.get("run", ""))]
        self.assertEqual(len(sets_dir), 1)
        self.assertIn("GITHUB_ENV", sets_dir[0]["run"])
        self.assertEqual(sets_dir[0]["if"], "matrix.language == 'rust' && steps.reuse.outputs.sarif == ''")

    def test_rust_reuses_main_results_only_when_chosen(self) -> None:
        self.assertEqual(
            self.wf["jobs"]["languages"]["outputs"]["reuse-rust"], "${{ steps.choose.outputs.reuse-rust }}"
        )
        job = self.wf["jobs"]["analyze"]
        steps = job["steps"]
        self.assertEqual(steps[0]["with"]["fetch-depth"], 2)
        self.assertEqual(job["permissions"]["actions"], "read")
        find = next(s for s in steps if s.get("id") == "reuse")
        self.assertIn("matrix.language == 'rust'", find["if"])
        self.assertIn("github.event_name == 'pull_request'", find["if"])
        self.assertIn("needs.languages.outputs.reuse-rust == 'true'", find["if"])
        self.assertIn("scripts/codeql_reuse.py", find["run"])
        self.assertIn("git rev-parse HEAD^1", find["run"])
        self.assertNotIn("${{", find["run"])
        upload = next(s for s in steps if str(s.get("uses", "")).startswith("github/codeql-action/upload-sarif@"))
        self.assertEqual(upload["if"], "steps.reuse.outputs.sarif != ''")
        self.assertEqual(upload["with"]["category"], "/language:rust")
        self.assertEqual(upload["with"]["sarif_file"], "${{ steps.reuse.outputs.sarif }}")
        init = next(s for s in steps if str(s.get("uses", "")).startswith("github/codeql-action/init@"))
        self.assertEqual(init["uses"].split("@")[1], upload["uses"].split("@")[1])
        # Reuse comes first, and every analysis step stands down for it.
        self.assertLess(steps.index(upload), steps.index(init))
        for step in steps[steps.index(upload) + 1 :]:
            if step.get("name") in ("Keep the Rust results for pull requests", "Save the Rust extractor's cargo output"):
                continue
            with self.subTest(step=step.get("name")):
                self.assertIn("steps.reuse.outputs.sarif == ''", step.get("if", ""))

    def test_main_keeps_the_rust_results(self) -> None:
        steps = self.wf["jobs"]["analyze"]["steps"]
        analyze = next(s for s in steps if str(s.get("uses", "")).startswith("github/codeql-action/analyze@"))
        self.assertEqual(analyze["with"]["output"], "${{ runner.temp }}/sarif")
        keep = next(s for s in steps if s.get("name") == "Keep the Rust results for pull requests")
        self.assertTrue(keep["uses"].startswith("actions/upload-artifact@"))
        self.assertIn("matrix.language == 'rust'", keep["if"])
        self.assertIn("github.ref == 'refs/heads/main'", keep["if"])
        self.assertIn("github.event_name != 'pull_request'", keep["if"])
        reuse = _load_reuse()
        self.assertEqual(keep["with"]["name"], reuse.ARTIFACT)
        self.assertEqual(keep["with"]["if-no-files-found"], "error")
        self.assertEqual(keep["with"]["path"], "${{ runner.temp }}/sarif/*.sarif")
        self.assertEqual(self.wf["jobs"]["analyze"]["name"].replace("${{ matrix.language }}", "rust"), reuse.RUST_JOB)
        self.assertEqual(WORKFLOW.name, reuse.WORKFLOW_FILE)

    def test_actions_are_pinned_and_permissions_are_narrow(self) -> None:
        self.assertEqual(self.wf["permissions"], {})
        for name, job in self.wf["jobs"].items():
            self.assertIn("permissions", job, name)
            for step in job["steps"]:
                if "uses" in step:
                    self.assertRegex(step["uses"], r"@[0-9a-f]{40}$", f"{name}: {step['uses']}")
                if str(step.get("uses", "")).startswith("actions/checkout@"):
                    self.assertIs(step["with"]["persist-credentials"], False, name)


if __name__ == "__main__":
    unittest.main()
