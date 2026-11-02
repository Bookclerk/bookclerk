#!/usr/bin/env python3
"""Tests for scripts/mutation_base.py: the daily mutation runs' starting commit.

The previous run's commit comes from the GitHub API, so ``choose`` only
accepts a full SHA that git confirms is an ancestor of ``HEAD``; the tests
drive it with fake answers, seeded mutations of ``mutation_base_corpus/``
checked against an independent oracle, and a real repository.
"""

from __future__ import annotations

import importlib.util
import os
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
    spec = importlib.util.spec_from_file_location("mutation_base", ROOT / "scripts" / "mutation_base.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutation_base"] = module
    spec.loader.exec_module(module)
    return module


mb = _load()
#: Seed answers for the API's ``head_sha``, valid and malformed.
CORPUS = Path(__file__).resolve().parent / "mutation_base_corpus"
A, B = "a" * 40, "b" * 40


class ChooseTest(unittest.TestCase):
    """The first full SHA that is an ancestor wins; anything else is skipped."""

    def test_order_and_validation(self) -> None:
        ancestors = {A, B}
        self.assertEqual(mb.choose([A, B], ancestors.__contains__), A)
        self.assertEqual(mb.choose(["", B], ancestors.__contains__), B)
        self.assertEqual(mb.choose(["c" * 40, B], ancestors.__contains__), B)
        for bad in ("A" * 40, "a" * 39, "-" + "a" * 39, "a" * 40 + "\n" + "b", "HEAD", "../x"):
            with self.subTest(bad=bad):
                self.assertIsNone(mb.choose([bad], lambda _: True))
        self.assertIsNone(mb.choose([], lambda _: True))

    def test_fuzz_only_full_shas_reach_git(self) -> None:
        """Seeded mutations of the corpus, checked against an independent oracle.

        The API's answer is untrusted text: ``choose`` must ask git only about
        40 lowercase hex characters and pick exactly the first candidate the
        oracle accepts that git confirms.
        """
        rng = random.Random(589)
        seeds = [path.read_bytes() for path in sorted(CORPUS.iterdir())]
        self.assertGreaterEqual(len(seeds), 8)
        accepted_any = False
        for case in range(3000):
            candidates = [_mutate(rng, rng.choice(seeds)).decode("utf-8", errors="replace") for _ in range(rng.randrange(1, 4))]
            known = {c.strip() for c in candidates if rng.random() < 0.7}
            asked: list[str] = []

            def is_ancestor(sha: str, asked: list[str] = asked, known: set[str] = known) -> bool:
                asked.append(sha)
                return sha in known

            chosen = mb.choose(candidates, is_ancestor)
            with self.subTest(case=case):
                for sha in asked:
                    self.assertTrue(_oracle_sha(sha), sha)
                expected = next((c.strip() for c in candidates if _oracle_sha(c.strip()) and c.strip() in known), None)
                self.assertEqual(chosen, expected)
            accepted_any = accepted_any or chosen is not None
        # The mutations keep enough valid SHAs that acceptance is exercised.
        self.assertTrue(accepted_any)

    def test_replay_corpus(self) -> None:
        """Each seed alone, against the oracle, with git confirming everything."""
        for path in sorted(CORPUS.iterdir()):
            text = path.read_bytes().decode("utf-8", errors="replace")
            with self.subTest(seed=path.name):
                expected = text.strip() if _oracle_sha(text.strip()) else None
                self.assertEqual(mb.choose([text], lambda _: True), expected)
        self.assertEqual(mb.choose([(CORPUS / "sha__full.txt").read_text(encoding="utf-8")], lambda _: True), "0123456789abcdef0123456789abcdef01234567")
        self.assertEqual(mb.choose([(CORPUS / "sha__padded.txt").read_text(encoding="utf-8")], lambda _: True), "0123456789abcdef0123456789abcdef01234567")
        for name in ("sha__upper.txt", "sha__short.txt", "sha__long.txt", "sha__option.txt", "sha__two_lines.txt", "sha__ref.txt"):
            self.assertIsNone(mb.choose([(CORPUS / name).read_text(encoding="utf-8")], lambda _: True), name)


def _oracle_sha(text: str) -> bool:
    """Independent check: exactly 40 characters, each a lowercase hex digit."""
    return len(text) == 40 and all(c in "0123456789abcdef" for c in text)


def _mutate(rng: random.Random, data: bytes) -> bytes:
    """Apply one to three seeded byte-level mutations to a corpus seed."""
    out = bytearray(data)
    for _ in range(rng.randrange(0, 4)):
        op = rng.randrange(6)
        if op == 0 and out:
            del out[rng.randrange(len(out))]
        elif op == 1:
            out.insert(rng.randrange(len(out) + 1), rng.choice(b"0123456789abcdefABCDEFg-/ \n\x00\xff"))
        elif op == 2 and out:
            i = rng.randrange(len(out))
            out[i] = rng.choice(b"0123456789abcdef")
        elif op == 3 and out:
            i = rng.randrange(len(out))
            out[i] ^= 1 << rng.randrange(8)
        elif op == 4:
            out = bytearray(bytes(out).swapcase()) if rng.random() < 0.5 else out
        elif op == 5 and out:
            out = out[: rng.randrange(len(out) + 1)]
    return bytes(out)


class MainTest(unittest.TestCase):
    """In a real repository: a manual base, the API's answer, then the fallbacks."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        repo = Path(self.tmp.name)
        git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"]
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        self.commits = []
        for n in range(3):
            (repo / "f.txt").write_text(str(n), encoding="utf-8")
            subprocess.run([*git, "add", "."], check=True)
            subprocess.run([*git, "commit", "-qm", str(n)], check=True)
            self.commits.append(subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip())
        cwd = Path.cwd()
        os.chdir(repo)
        self.addCleanup(os.chdir, cwd)

    def run_main(self, argv: list[str], api: str = "") -> tuple[int, str]:
        with mock.patch.object(mb, "_last_success", return_value=api), mock.patch("sys.stdout") as stdout, mock.patch("sys.stderr"):
            code = mb.main(argv)
        return code, "".join(c.args[0] for c in stdout.write.call_args_list)

    def test_sources_in_order(self) -> None:
        first, second, head = self.commits
        self.assertEqual(self.run_main(["--workflow", "mutation.yml", "--base", first[:10]], api=second), (0, first + "\n"))
        self.assertEqual(self.run_main(["--workflow", "mutation.yml"], api=first), (0, first + "\n"))
        # Nothing merged since the last run: an empty range, not a retest.
        self.assertEqual(self.run_main(["--workflow", "mutation.yml"], api=head), (0, head + "\n"))
        # A commit git does not have, or junk: the fallbacks.
        for api in ("f" * 40, "not a sha"):
            with self.subTest(api=api):
                self.assertEqual(self.run_main(["--workflow", "mutation.yml"], api=api), (0, second + "\n"))
        self.assertEqual(self.run_main(["--workflow", "../x.yml"])[0], 1)
        self.assertEqual(self.run_main(["--workflow", "mutation.yml", "--base=--all"], api=first), (0, first + "\n"))

    def test_last_success_rejects_odd_environments(self) -> None:
        with mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "not a repo"}):
            self.assertEqual(mb._last_success("mutation.yml"), "")


if __name__ == "__main__":
    unittest.main()
