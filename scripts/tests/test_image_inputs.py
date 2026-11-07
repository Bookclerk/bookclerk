#!/usr/bin/env python3
"""Tests for ``scripts/image_inputs.py``: what images are made of.

The ``app`` inputs leave out paths no build reads. Leaving out a path a
build does read would publish a stale image, so these tests also check the
repository: no crate embeds a file from an excluded path, and no excluded
path is a workspace member.
"""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


ii = _load("image_inputs", ROOT / "scripts" / "image_inputs.py")

#: ``include_str!("...")`` and its relatives, with the path they embed.
INCLUDE = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"')


def _excluded(rel: str) -> bool:
    """Whether a repository-relative path is outside the ``app`` inputs."""
    if "/" not in rel and rel.endswith(".md"):
        return True
    return any(rel == p or rel.startswith(p + "/") for p in ii.APP_EXCLUDES)


class RepositoryTests(unittest.TestCase):
    """The ``app`` exclusions against what the build actually reads."""

    def test_no_crate_embeds_an_excluded_file(self) -> None:
        offenders = []
        for top in ("crates", "plugins", "packages", "examples", "third_party", "tests"):
            base = ROOT / top
            if not base.is_dir():
                continue
            for src in base.rglob("*.rs"):
                if "target" in src.parts or "node_modules" in src.parts:
                    continue
                for target in INCLUDE.findall(src.read_text(encoding="utf-8", errors="replace")):
                    if "$" in target or "{" in target:
                        continue
                    resolved = os.path.normpath(src.parent / target)
                    try:
                        rel = Path(resolved).relative_to(ROOT).as_posix()
                    except ValueError:
                        continue
                    if _excluded(rel):
                        offenders.append(f"{src.relative_to(ROOT)} embeds {rel}")
        self.assertEqual(offenders, [])

    def test_no_workspace_member_is_excluded(self) -> None:
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
        for member in workspace.get("members", []):
            self.assertFalse(_excluded(member.rstrip("/*")), member)

    def test_the_docker_build_reads_no_excluded_path(self) -> None:
        for dockerfile in ("packaging/docker/Dockerfile", "packaging/demo/Dockerfile"):
            lines = (ROOT / dockerfile).read_text(encoding="utf-8").splitlines()
            text = "\n".join(line for line in lines if not line.lstrip().startswith("#"))
            for path in ii.APP_EXCLUDES:
                self.assertNotIn(f" {path}/", text, f"{dockerfile} reads {path}")

    def test_specs(self) -> None:
        (app,) = ii.spec("app")
        self.assertEqual(app[0], ".")
        self.assertIn(":(exclude)docs", app)
        self.assertNotIn("crates", " ".join(app))
        demo = ii.spec("demo")
        self.assertEqual(demo[0], app)
        self.assertIn("tools/demo", demo[1])
        self.assertIn(".github/workflows/demo-deploy.yml", demo[1])
        self.assertIn(".devcontainer", ii.spec("dev")[0])
        with self.assertRaises(ii.InputsError):
            ii.spec("bookclerk")


class GitTests(unittest.TestCase):
    """``candidates`` and ``unchanged`` on a scratch repository."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self._tmp.name)
        self._cwd = os.getcwd()
        os.chdir(self.repo)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "test")
        self.git("config", "commit.gpgsign", "false")

    def tearDown(self) -> None:
        os.chdir(self._cwd)
        self._tmp.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, path: str, text: str = "x") -> str:
        """Writes ``path`` (different content every call) and commits it."""
        self.n = getattr(self, "n", 0) + 1
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(f"{text} {self.n}\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", path)
        return self.git("rev-parse", "HEAD")

    def test_candidates_stop_at_the_last_input_change(self) -> None:
        self.commit("crates/a/lib.rs")
        change = self.commit("crates/a/lib.rs", "y")
        docs = self.commit("docs/page.md")
        root_md = self.commit("README.md")
        worker = self.commit("tools/demo/src/app.ts")
        self.assertEqual(ii.candidates("app", worker), [worker, root_md, docs, change])
        # The Worker is a demo input, not an app input.
        self.assertEqual(ii.candidates("demo", worker), [worker])
        self.assertEqual(ii.candidates("app", change), [change])

    def test_a_crate_readme_is_an_input(self) -> None:
        self.commit("crates/a/lib.rs")
        readme = self.commit("crates/a/README.md")
        self.assertEqual(ii.candidates("app", readme), [readme])

    def test_candidates_are_capped(self) -> None:
        self.commit("crates/a/lib.rs")
        for n in range(ii.MAX_CANDIDATES + 3):
            head = self.commit(f"docs/{n}.md")
        found = ii.candidates("app", head)
        self.assertEqual(len(found), ii.MAX_CANDIDATES)
        self.assertEqual(found[0], head)

    def test_a_change_merged_through_a_second_parent_is_not_skipped(self) -> None:
        base = self.commit("crates/a/lib.rs")
        self.git("checkout", "-q", "-b", "side")
        self.commit("crates/a/lib.rs", "side")
        self.git("checkout", "-q", "main")
        before = self.commit("docs/a.md")
        self.git("merge", "-q", "--no-ff", "-m", "merge", "side")
        merge = self.git("rev-parse", "HEAD")
        self.assertEqual(ii.candidates("app", merge), [merge])
        self.assertFalse(ii.unchanged("app", before, merge))
        self.assertFalse(ii.unchanged("app", base, merge))

    def test_unchanged(self) -> None:
        first = self.commit("crates/a/lib.rs")
        docs = self.commit("docs/a.md")
        worker = self.commit("tools/demo/src/app.ts")
        self.assertTrue(ii.unchanged("app", first, worker))
        self.assertFalse(ii.unchanged("demo", first, worker))
        self.assertTrue(ii.unchanged("demo", worker, worker))
        self.assertTrue(ii.unchanged("demo", first, docs))
        # Backwards is not "unchanged": the live demo would go back.
        self.assertFalse(ii.unchanged("app", worker, first))
        self.assertFalse(ii.unchanged("app", "0" * 40, worker))

    def test_dev_inputs(self) -> None:
        self.commit(".devcontainer/Dockerfile")
        crate = self.commit("crates/a/lib.rs")
        tools = self.commit(".tool-versions")
        self.assertEqual(ii.candidates("dev", crate)[-1], self.git("rev-parse", "HEAD~2"))
        self.assertEqual(ii.candidates("dev", tools), [tools])

    def test_bad_arguments(self) -> None:
        head = self.commit("crates/a/lib.rs")
        for bad in ("HEAD", head.upper(), head[:12], "--all", head + "\n"):
            with self.assertRaises(ii.InputsError, msg=bad):
                ii.candidates("app", bad)
            with self.assertRaises(ii.InputsError, msg=bad):
                ii.unchanged("app", bad, head)

    def test_main(self) -> None:
        head = self.commit("crates/a/lib.rs")
        self.assertEqual(ii.main(["candidates", "app", head]), 0)
        self.assertEqual(ii.main(["unchanged", "app", head, head]), 0)
        self.assertEqual(ii.main(["unchanged", "app", "0" * 40, head]), 1)
        self.assertEqual(ii.main(["candidates", "app", "HEAD"]), 2)
        self.assertEqual(ii.main(["candidates", "app", "f" * 40]), 1)
        self.assertEqual(ii.main([]), 2)


if __name__ == "__main__":
    unittest.main()
