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

# The oracle's own reading of a Rust input, written without fnmatch.
ORACLE_NAMES = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"}
ORACLE_SUFFIXES = (".rs", ".capnp")
ORACLE_EXACT = {".github/workflows/codeql.yml", "scripts/codeql_languages.py"}


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
        if path.startswith(".cargo/") or path in ORACLE_EXACT:
            return True
    return False


def analyzes_rust(raw: bytes, event: str = "pull_request") -> bool:
    return cl.RUST in cl.choose(event, cl.parse_paths(raw)).languages


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

    def test_matrix_is_build_mode_none_for_every_language(self) -> None:
        doc = json.loads(cl.matrix_json(cl.choose("pull_request", ["docs/a.md"])))
        self.assertEqual([e["language"] for e in doc["include"]], list(cl.ALWAYS))
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
                data[pos:pos] = rng.choice([b".rs", b"\x00", b"Cargo.toml", b".cargo/", b".capnp", b"/"])
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

    def run_main(self, event: str, raw: bytes | None) -> dict:
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
            line = out.read_text(encoding="utf-8")
            self.assertTrue(line.startswith("matrix=") and line.endswith("\n"))
            self.assertIn("CodeQL languages:", summary.read_text(encoding="utf-8"))
            return json.loads(line[len("matrix=") :])

    def test_pull_request_without_rust(self) -> None:
        langs = [e["language"] for e in self.run_main("pull_request", b"docs/a.md\x00")["include"]]
        self.assertNotIn("rust", langs)

    def test_pull_request_with_rust(self) -> None:
        langs = [e["language"] for e in self.run_main("pull_request", b"crates/a/src/lib.rs\x00")["include"]]
        self.assertIn("rust", langs)

    def test_push_needs_no_paths(self) -> None:
        langs = [e["language"] for e in self.run_main("push", None)["include"]]
        self.assertIn("rust", langs)

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
