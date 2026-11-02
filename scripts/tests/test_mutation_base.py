#!/usr/bin/env python3
"""Tests for scripts/mutation_base.py: the daily mutation runs' starting commit.

The previous run's commit comes from the GitHub API, so ``choose`` only
accepts a full SHA that git confirms is an ancestor of ``HEAD``; the tests
drive it with fake answers and with a real repository.
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
        """Oracle: git is only asked about 40 lowercase hex characters."""
        rng = random.Random(589)
        alphabet = "0123456789abcdefABCDEF-./ \n\x00g"
        for _ in range(500):
            candidates = ["".join(rng.choice(alphabet) for _ in range(rng.choice([0, 7, 39, 40, 41]))) for _ in range(rng.randrange(0, 4))]
            asked: list[str] = []

            def is_ancestor(sha: str, asked: list[str] = asked) -> bool:
                asked.append(sha)
                return rng.random() < 0.5

            chosen = mb.choose(candidates, is_ancestor)
            for sha in asked:
                self.assertRegex(sha, r"^[0-9a-f]{40}$")
            if chosen is not None:
                self.assertEqual(chosen, asked[-1])


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
        # HEAD itself, a commit git does not have, or junk: the fallbacks.
        for api in (head, "f" * 40, "not a sha"):
            with self.subTest(api=api):
                self.assertEqual(self.run_main(["--workflow", "mutation.yml"], api=api), (0, second + "\n"))
        self.assertEqual(self.run_main(["--workflow", "../x.yml"])[0], 1)
        self.assertEqual(self.run_main(["--workflow", "mutation.yml", "--base=--all"], api=first), (0, first + "\n"))

    def test_last_success_rejects_odd_environments(self) -> None:
        with mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "not a repo"}):
            self.assertEqual(mb._last_success("mutation.yml"), "")


if __name__ == "__main__":
    unittest.main()
