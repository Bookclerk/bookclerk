#!/usr/bin/env python3
"""Guardrails for scripts/ci_plan/relations.toml.

These scans recognize *literal* reference forms only:

- ``include_str!`` / ``include_bytes!`` paths that leave their package;
- string literals in Rust sources that resolve to another package or a
  repository path (``"../../x"``, ``join("crates/...")``);
- workspace binary names quoted in ``tests/`` sources.

Each recognized reference must be declared (embed / test_input / fixture /
test_guests), already related through Cargo dependencies, under a
full-suite path, or listed under ``[[reviewed]]`` with a reason. Computed
paths, macro-generated includes and dynamically discovered fixtures are not
detected; declare those explicitly (see docs/ci.md).
"""

from __future__ import annotations

import json
import os
import re
import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
REPO = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

from ci_plan.plan import (  # noqa: E402
    FULL_SUITE_PATH_PREFIXES,
    glob_match,
    load_relations,
    package_index_from_metadata,
)

META = json.loads((SCRIPTS / "tests/ci_plan_fixtures/workspace_metadata.json").read_text())
INDEX = package_index_from_metadata(META)
REL = load_relations()
TOP_DIRS = ("crates", "examples", "packages", "ui", "assets", "scripts", "tools", "docs", "third_party", "fuzz")

INCLUDE = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"')
REL_LITERAL = re.compile(r'"(\.\./[^"\\]*)"')
JOIN_LITERAL = re.compile(r'join\(\s*"((?:%s)/[^"\\]*)"\s*\)' % "|".join(TOP_DIRS))


def _deps(pkg: str) -> set[str]:
    out = set()
    for p in META["packages"]:
        if p["name"] == pkg:
            out = {d["name"] for d in p["dependencies"] if d.get("path")}
    return out


def _declared_globs(pkg: str) -> list[str]:
    globs: list[str] = []
    for e in REL.embeds + REL.test_inputs:
        if e["package"] == pkg:
            globs += e["paths"]
    return globs


def _glob_base(pattern: str) -> str:
    idx = min((pattern.find(c) for c in "*?" if c in pattern), default=len(pattern))
    return pattern[:idx].rstrip("/")


def _reviewed(pkg: str, ref: str) -> bool:
    return any(r["package"] == pkg and r["reference"] == ref for r in REL.reviewed)


def _owner_dir(path: str) -> str | None:
    for d, name in INDEX.dirs:
        if path == d:
            return name
    return None


def _covered(pkg: str, ref: str, literal: str) -> bool:
    if any(ref.startswith(p) or ref + "/" == p for p in FULL_SUITE_PATH_PREFIXES):
        return True
    if _reviewed(pkg, literal) or _reviewed(pkg, ref):
        return True
    for g in _declared_globs(pkg):
        base = _glob_base(g)
        if glob_match(ref, g) or ref.startswith(base + "/") or ref == base or base.startswith(ref + "/"):
            return True
    owner = _owner_dir(ref)
    guests = set(REL.test_guests.get(pkg, {}).get("build", []))
    if owner is not None and (owner in guests or owner in _deps(pkg)):
        return True
    return False


def _rust_files(pkg_dir: Path):
    for dp, dirs, files in os.walk(pkg_dir):
        dirs[:] = [d for d in dirs if d not in ("target", "node_modules")]
        for f in files:
            if f.endswith(".rs"):
                yield Path(dp) / f


def _rel(p: Path) -> str:
    return os.path.relpath(p, REPO).replace(os.sep, "/")


class RelationsGuardrailTests(unittest.TestCase):
    def test_include_paths_leaving_a_package_are_declared(self) -> None:
        problems = []
        for name, info in INDEX.by_name.items():
            pkg_dir = REPO / info.manifest_dir
            for f in _rust_files(pkg_dir):
                for lit in INCLUDE.findall(f.read_text(errors="ignore")):
                    target = _rel((f.parent / lit).resolve())
                    if target.startswith(info.manifest_dir + "/"):
                        continue
                    if not _covered(name, target, lit):
                        problems.append(f"{name}: {_rel(f)} includes {target}")
        self.assertEqual(problems, [], "declare these as [[embed]] / [[test_input]] in relations.toml")

    def test_repository_path_literals_are_declared(self) -> None:
        problems = []
        for name, info in INDEX.by_name.items():
            pkg_dir = REPO / info.manifest_dir
            for f in _rust_files(pkg_dir):
                text = f.read_text(errors="ignore")
                refs = [(lit, _rel((pkg_dir / lit).resolve())) for lit in REL_LITERAL.findall(text)]
                refs += [(lit, lit.rstrip("/")) for lit in JOIN_LITERAL.findall(text)]
                for lit, target in refs:
                    if target.startswith("..") or not (REPO / target).exists():
                        continue  # not a repository path (e.g. traversal test data)
                    if target == "." or info.manifest_dir.startswith(target + "/"):
                        continue  # repo root / ancestor helpers
                    if target == info.manifest_dir or target.startswith(info.manifest_dir + "/"):
                        continue
                    if not _covered(name, target, lit):
                        problems.append(f"{name}: {_rel(f)} references {target} ({lit!r})")
        self.assertEqual(problems, [], "declare in relations.toml or add a [[reviewed]] entry with a reason")

    def test_workspace_binaries_named_in_tests_are_declared(self) -> None:
        bins = {}
        for p in META["packages"]:
            for t in p["targets"]:
                if "bin" in t["kind"]:
                    bins[t["name"]] = p["name"]
        problems = []
        for name, info in INDEX.by_name.items():
            tests_dir = REPO / info.manifest_dir / "tests"
            if not tests_dir.is_dir():
                continue
            for f in _rust_files(tests_dir):
                text = f.read_text(errors="ignore")
                for b, owner in bins.items():
                    if owner == name or b == "bookclerk":
                        continue
                    if re.search(r'"(?:\./)?%s"' % re.escape(b), text):
                        guests = set(REL.test_guests.get(name, {}).get("build", []))
                        if owner not in guests and owner not in _deps(name) and not _reviewed(name, b):
                            problems.append(f"{name}: {_rel(f)} names {b} (package {owner})")
        self.assertEqual(problems, [], "add to [test_guests.<package>].build or [[reviewed]]")

    def test_declared_paths_exist(self) -> None:
        files = [
            _rel(Path(dp) / f)
            for dp, dirs, fs in os.walk(REPO)
            if not any(part in (".git", "target", "node_modules", ".cargo-home", ".tmp") for part in Path(dp).parts)
            for f in fs
        ]
        for section in (REL.embeds, REL.test_inputs, REL.fixtures):
            for entry in section:
                for g in entry["paths"]:
                    with self.subTest(glob=g):
                        self.assertTrue(any(glob_match(f, g) for f in files), f"{g} matches nothing")

    def test_test_guests_are_binaries(self) -> None:
        has_bin = {p["name"] for p in META["packages"] if any("bin" in t["kind"] for t in p["targets"])}
        for pkg, spec in REL.test_guests.items():
            for b in spec.get("build", []):
                self.assertIn(b, has_bin, f"test_guests.{pkg}: {b} has no binary target")

    def test_fixture_dirs_are_not_compiled_by_their_owner(self) -> None:
        for entry in REL.fixtures:
            for g in entry["paths"]:
                base = _glob_base(g)
                owner = next(n for d, n in INDEX.dirs if base.startswith(d + "/"))
                owner_dir = REPO / INDEX.by_name[owner].manifest_dir
                leaf = base[len(INDEX.by_name[owner].manifest_dir) + 1 :]
                for f in _rust_files(owner_dir):
                    for lit in INCLUDE.findall(f.read_text(errors="ignore")):
                        target = _rel((f.parent / lit).resolve())
                        self.assertFalse(
                            target.startswith(base),
                            f"{owner} compiles fixture {target}; drop it from [[fixture]]",
                        )
                    self.assertNotIn(f'"{leaf}/', f.read_text(errors="ignore"), f"{_rel(f)} reads {leaf}")

    def test_macro_built_include_paths_are_reviewed(self) -> None:
        # `include_str!(concat!(env!(..), "..."))` hides the path from the
        # literal scan above; each use must be reviewed explicitly.
        macro_include = re.compile(r'include_(?:str|bytes)!\(\s*concat!\((?:[^()]|\([^()]*\))*?"([^"]+)"')
        problems = []
        for name, info in INDEX.by_name.items():
            for f in _rust_files(REPO / info.manifest_dir):
                for tail in macro_include.findall(f.read_text(errors="ignore")):
                    target = _rel((REPO / info.manifest_dir / tail.lstrip("/")).resolve())
                    if not (_covered(name, target, tail) or _reviewed(name, target)):
                        problems.append(f"{name}: {_rel(f)} includes {target} via concat!")
        self.assertEqual(problems, [], "declare or add a [[reviewed]] entry with a reason")

    def test_reviewed_entries_have_reasons(self) -> None:
        for r in REL.reviewed:
            self.assertTrue(r.get("reason"), r)


if __name__ == "__main__":
    unittest.main()
