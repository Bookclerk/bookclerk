#!/usr/bin/env python3
"""OSV advisories: shipped-artifact map, evidence, verdicts and filing (offline)."""

from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import sys
import tempfile
import unittest
import urllib.error
from email.message import Message
from pathlib import Path
from types import ModuleType
from typing import Any

import tomllib
import yaml

ROOT = Path(__file__).resolve().parents[2]
LOCKFILE_NAMES = (
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
)


def _load() -> ModuleType:
    path = ROOT / "scripts" / "osv-advisories.py"
    spec = importlib.util.spec_from_file_location("osv_advisories", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["osv_advisories"] = module
    spec.loader.exec_module(module)
    return module


adv = _load()


def _record(vuln_id: str, name: str, ecosystem: str, fixed: str, **extra: Any) -> dict[str, Any]:
    affected: dict[str, Any] = {
        "package": {"ecosystem": ecosystem, "name": name},
        "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": fixed}]}],
    }
    if "functions" in extra:
        affected["ecosystem_specific"] = {"affects": {"functions": extra.pop("functions")}}
    return {"id": vuln_id, "affected": [affected], **extra}


CARGO_LOCK = """
version = 4

[[package]]
name = "smallvec"
version = "1.6.0"

[[package]]
name = "time"
version = "0.1.0"

[[package]]
name = "devonly"
version = "1.0.0"
"""

UI_LOCK = {
    "lockfileVersion": 3,
    "packages": {
        "": {"name": "ui"},
        "node_modules/react": {"version": "19.0.0"},
        "node_modules/tailwind-merge": {"version": "3.0.0"},
        "node_modules/vite": {"version": "8.0.0", "dev": True},
        "node_modules/@scope/lib": {"version": "1.0.0"},
        "node_modules/@scope/lib/node_modules/inner": {"version": "2.0.0"},
    },
}

SDK_LOCK = {
    "lockfileVersion": 3,
    "packages": {
        "": {"name": "@bookclerk/plugin-sdk"},
        "node_modules/smol-toml": {"version": "1.0.0"},
        "node_modules/esbuild": {"version": "0.1.0", "dev": True},
    },
}

CONFIG = """
not_shipped = [{ path = "fuzz/Cargo.lock", why = "ci" }]

[artifacts.bookclerk]
title = "Host"
ecosystem = "other"
package = "bookclerk"
version = "release-tag"

[artifacts.rust-sdk]
title = "Rust SDK"
ecosystem = "other"
package = "bookclerk-plugin-sdk"
version = "release-tag"

[artifacts.plugin-sdk-npm]
title = "TS SDK"
ecosystem = "npm"
package = "@bookclerk/plugin-sdk"
version = "packages/plugin-sdk/package.json"

[[shipped]]
artifact = "bookclerk"
lockfile = "Cargo.lock"
evidence = "cargo"
default_members = true
call_analysis = true

[[shipped]]
artifact = "rust-sdk"
lockfile = "Cargo.lock"
evidence = "cargo"
roots = ["sdk"]

[[shipped]]
artifact = "bookclerk"
lockfile = "ui/package-lock.json"
evidence = "npm-bundle"

[[shipped]]
artifact = "plugin-sdk-npm"
lockfile = "packages/plugin-sdk/package-lock.json"
evidence = "npm"
"""


class FakeGitHub:
    """Records advisories instead of calling the API."""

    def __init__(self) -> None:
        self.created: list[dict[str, Any]] = []
        self.updated: list[tuple[str, dict[str, Any]]] = []

    def create_advisory(self, body: dict[str, Any]) -> str:
        self.created.append(body)
        return f"GHSA-fake-{len(self.created)}"

    def update_advisory(self, ghsa_id: str, body: dict[str, Any]) -> None:
        self.updated.append((ghsa_id, body))


class Repo:
    """A temporary repository root with the fixture lockfiles and map."""

    def __init__(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        (self.root / "Cargo.lock").write_text(CARGO_LOCK, encoding="utf-8")
        (self.root / "ui").mkdir()
        (self.root / "ui" / "package-lock.json").write_text(json.dumps(UI_LOCK), encoding="utf-8")
        sdk = self.root / "packages" / "plugin-sdk"
        sdk.mkdir(parents=True)
        (sdk / "package-lock.json").write_text(json.dumps(SDK_LOCK), encoding="utf-8")
        (sdk / "package.json").write_text(json.dumps({"version": "0.3.0"}), encoding="utf-8")
        (self.root / "shipped-artifacts.toml").write_text(CONFIG, encoding="utf-8")
        self.config = adv.load_config(self.root / "shipped-artifacts.toml")

    def close(self) -> None:
        self._tmp.cleanup()


EVIDENCE = {
    "bookclerk|Cargo.lock": {"packages": [["smallvec", "1.6.0"], ["time", "0.1.0"]]},
    "rust-sdk|Cargo.lock": {"packages": [["time", "0.1.0"]]},
    "bookclerk|ui/package-lock.json": {"packages": [["react", "19.0.0"], ["tailwind-merge", "3.0.0"]]},
    "plugin-sdk-npm|packages/plugin-sdk/package-lock.json": {"packages": [["smol-toml", "1.0.0"]]},
}


def _group(lockfile: str, record: dict[str, Any]) -> Any:
    return adv.Group(lockfile, sorted(adv.record_ids(record)), record, ["code-scanning"])


class RepositoryMapTest(unittest.TestCase):
    """The real `shipped-artifacts.toml` stays complete and consistent."""

    def setUp(self) -> None:
        self.config = adv.load_config()

    def test_every_tracked_lockfile_is_classified(self) -> None:
        tracked = subprocess.run(["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True)
        lockfiles = {
            path
            for path in tracked.stdout.splitlines()
            if Path(path).name in LOCKFILE_NAMES
            or (Path(path).name.startswith("requirements") and path.endswith(".txt"))
        }
        shipped = {s.lockfile for s in self.config.shipped}
        unclassified = sorted(lockfiles - shipped - set(self.config.not_shipped))
        self.assertEqual(unclassified, [], "add each lockfile to shipped-artifacts.toml (shipped or not_shipped)")

    def test_listed_lockfiles_exist(self) -> None:
        for path in self.config.not_shipped:
            self.assertTrue((ROOT / path).is_file(), path)
        for entry in self.config.shipped:
            # The Python SDK's lockfile carries dev tooling only; the map
            # proves its runtime closure from the manifest instead.
            if entry.evidence != "pip":
                self.assertTrue((ROOT / entry.lockfile).is_file(), entry.lockfile)

    def test_fuzz_boundaries_name_the_same_artifacts(self) -> None:
        boundaries = tomllib.loads((ROOT / "fuzz" / "boundaries.toml").read_text(encoding="utf-8"))
        for key, spec in boundaries["artifacts"].items():
            self.assertIn(key, self.config.artifacts, key)
            artifact = self.config.artifacts[key]
            self.assertEqual((artifact.ecosystem, artifact.package), (spec["ecosystem"], spec["package"]), key)

    def test_python_sdk_has_no_runtime_dependencies(self) -> None:
        # Bookclerk's Python SDK ships with zero runtime dependencies, so no
        # dependency advisory can affect it. Adding one needs its closure
        # resolved in scripts/osv-advisories.py first; until then this fails.
        for entry in self.config.shipped:
            if entry.evidence == "pip":
                pyproject = tomllib.loads((ROOT / entry.manifest).read_text(encoding="utf-8"))
                self.assertEqual(adv.python_runtime_deps(pyproject, entry.dev_extras), [], entry.manifest)

    def test_cargo_roots_resolve(self) -> None:
        names = {p.name for p in (ROOT / "crates" / "bookclerk-plugins" / "platform").iterdir() if p.is_dir()}
        self.assertTrue(names, "platform guests directory is empty")
        for entry in self.config.shipped:
            for rel in entry.root_dirs:
                self.assertTrue((ROOT / rel).is_dir(), rel)

    def test_artifact_versions_resolve(self) -> None:
        for artifact in self.config.artifacts.values():
            self.assertTrue(adv.artifact_version(artifact, "v9.9.9", ROOT))


class ConfigTest(unittest.TestCase):
    def test_rejects_unknown_artifact_and_overlap(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.toml"
            path.write_text('[[shipped]]\nartifact = "nope"\nlockfile = "x"\nevidence = "npm"\n', encoding="utf-8")
            with self.assertRaises(ValueError):
                adv.load_config(path)
            path.write_text(
                CONFIG.replace('path = "fuzz/Cargo.lock"', 'path = "Cargo.lock"'),
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                adv.load_config(path)


class PythonGuardTest(unittest.TestCase):
    def test_runtime_dependencies_and_non_dev_extras_count(self) -> None:
        doc = {
            "project": {"dependencies": ["httpx>=0.27"], "optional-dependencies": {"dev": ["pytest"], "s3": ["boto3"]}}
        }
        self.assertEqual(adv.python_runtime_deps(doc, ("dev",)), ["httpx>=0.27", "boto3 (extra s3)"])
        self.assertEqual(adv.python_runtime_deps({"project": {"dependencies": []}}, ()), [])

    def test_evidence_refuses_a_runtime_dependency(self) -> None:
        repo = Repo()
        try:
            (repo.root / "pyproject.toml").write_text('[project]\ndependencies = ["requests"]\n', encoding="utf-8")
            config = adv.Config(
                artifacts=repo.config.artifacts,
                shipped=(adv.Shipped(repo.config.artifacts["bookclerk"], "uv.lock", "pip", manifest="pyproject.toml"),),
                not_shipped={},
            )
            with self.assertRaises(ValueError):
                adv.collect_evidence(config, repo.root, lambda _: {})
        finally:
            repo.close()


def _metadata() -> dict[str, Any]:
    def pkg(name: str, kind: str = "lib") -> dict[str, Any]:
        return {
            "id": f"{name}-id",
            "name": name,
            "version": "1.0.0",
            "manifest_path": f"/ws/crates/{name}/Cargo.toml",
            "targets": [{"kind": [kind]}],
        }

    def dep(name: str, *kinds: str | None) -> dict[str, Any]:
        return {"pkg": f"{name}-id", "dep_kinds": [{"kind": k} for k in kinds]}

    packages = [
        pkg("host", "bin"),
        pkg("guest", "bin"),
        pkg("normal"),
        pkg("deep"),
        pkg("testing"),
        pkg("builder"),
        pkg("derive", "proc-macro"),
        pkg("macro-only"),
        pkg("both"),
        pkg("openssl-src"),
        pkg("cc"),
    ]
    nodes = [
        {
            "id": "host-id",
            "deps": [
                dep("normal", None),
                dep("testing", "dev"),
                dep("builder", "build"),
                dep("derive", None),
                dep("both", "dev", None),
                dep("openssl-src", "build"),
            ],
        },
        {"id": "openssl-src-id", "deps": [dep("cc", None)]},
        {"id": "normal-id", "deps": [dep("deep", None)]},
        {"id": "derive-id", "deps": [dep("macro-only", None)]},
        {"id": "guest-id", "deps": []},
    ]
    return {
        "packages": packages,
        "resolve": {"nodes": nodes},
        "workspace_members": ["host-id", "guest-id"],
        "workspace_default_members": ["host-id"],
    }


class CargoEvidenceTest(unittest.TestCase):
    def test_closure_keeps_normal_edges_only(self) -> None:
        closure = {name for name, _ in adv.cargo_closure(_metadata(), {"host-id"})}
        # Vendored C compiled by a `-src` build dependency ships too, but not
        # the build tooling it depends on.
        self.assertEqual(closure, {"host", "normal", "deep", "both", "openssl-src"})

    def test_roots(self) -> None:
        meta = _metadata()
        artifact = adv.Artifact("a", "A", "other", "a", "release-tag")
        entry = adv.Shipped(artifact, "Cargo.lock", "cargo", default_members=True)
        self.assertEqual(adv.cargo_roots(meta, entry, Path("/ws")), {"host-id"})
        entry = adv.Shipped(artifact, "Cargo.lock", "cargo", root_dirs=("crates/guest",))
        self.assertEqual(adv.cargo_roots(meta, entry, Path("/ws")), {"guest-id"})
        with self.assertRaises(ValueError):
            adv.cargo_roots(meta, adv.Shipped(artifact, "Cargo.lock", "cargo", roots=("nope",)), Path("/ws"))
        with self.assertRaises(ValueError):
            adv.cargo_roots(meta, adv.Shipped(artifact, "Cargo.lock", "cargo", root_dirs=("empty",)), Path("/ws"))


class NpmEvidenceTest(unittest.TestCase):
    def test_production_drops_dev(self) -> None:
        self.assertEqual(
            adv.npm_production(UI_LOCK),
            {("react", "19.0.0"), ("tailwind-merge", "3.0.0"), ("@scope/lib", "1.0.0"), ("inner", "2.0.0")},
        )

    def test_lock_keys_for_sources(self) -> None:
        self.assertEqual(adv.lock_key_for_source("../node_modules/react/index.js"), "node_modules/react")
        self.assertEqual(
            adv.lock_key_for_source("../../node_modules/@scope/lib/node_modules/inner/dist/x.js"),
            "node_modules/@scope/lib/node_modules/inner",
        )
        self.assertIsNone(adv.lock_key_for_source("../src/main.tsx"))

    def test_bundle_packages_from_source_maps(self) -> None:
        maps = [
            {"sources": ["../src/App.tsx", "../node_modules/react/cjs/react.production.js"]},
            {"sources": ["../node_modules/@scope/lib/node_modules/inner/i.js", "\u0000vite/preload-helper"]},
        ]
        self.assertEqual(adv.bundle_packages(maps, UI_LOCK), {("react", "19.0.0"), ("inner", "2.0.0")})


class SymbolTest(unittest.TestCase):
    def test_clean_symbol(self) -> None:
        cases = {
            "smallvec::SmallVec<A>::grow::h0123456789abcdef": "smallvec::SmallVec::grow",
            "<smallvec[86b69f0c]::SmallVec<[u8; 2: usize]>>::grow": "smallvec::SmallVec::grow",
            "<libflate::gzip::MultiDecoder as std::io::Read>::read": "libflate::gzip::MultiDecoder::read",
            "<alloc::vec::Vec<u8> as core::ops::drop::Drop>::drop": "alloc::vec::Vec::drop",
            "core[940e]::ptr::drop_glue::<smallvec[86b]::SmallVec<[u8; 2: usize]>>": "core::ptr::drop_glue",
            "<core::result::Result<a::B, c::D>>::unwrap": "core::result::Result::unwrap",
        }
        for raw, want in cases.items():
            self.assertEqual(adv.clean_symbol(raw), want, raw)

    def test_linkage_names(self) -> None:
        lines = [
            "    <d71>   DW_AT_linkage_name: (strp) (offset: 0x4f55a): _RNvMsc_CsIOFwI9U1So_8smallvec4grow",
            "    <8f>   DW_AT_linkage_name: (indirect string, offset: 0x2a1): _ZN4core3fmt5write17h0123456789abcdefE",
            "    <d75>   DW_AT_name        : (strp) (offset: 0x1caba): grow<[u8; 2]>",
        ]
        self.assertEqual(
            adv.linkage_names(lines),
            {"_RNvMsc_CsIOFwI9U1So_8smallvec4grow", "_ZN4core3fmt5write17h0123456789abcdefE"},
        )


class GroupingTest(unittest.TestCase):
    def test_aliases_merge_across_sources(self) -> None:
        rustsec = _record("RUSTSEC-2021-0003", "smallvec", "crates.io", "1.6.1", aliases=["GHSA-43w2-9j62-hq99"])
        ghsa = _record("GHSA-43w2-9j62-hq99", "smallvec", "crates.io", "1.6.1", aliases=["RUSTSEC-2021-0003", "CVE-1"])
        alerts = [
            ("RUSTSEC-2021-0003", "Cargo.lock", "code-scanning"),
            ("GHSA-43w2-9j62-hq99", "Cargo.lock", "dependabot"),
            ("GHSA-43w2-9j62-hq99", "fuzz/Cargo.lock", "dependabot"),
            ("DEPRECATED-1", "Cargo.lock", "code-scanning"),
        ]
        groups, unknown = adv.group_alerts(
            alerts, {"RUSTSEC-2021-0003": rustsec, "GHSA-43w2-9j62-hq99": ghsa, "DEPRECATED-1": None}
        )
        self.assertEqual(unknown, 1)
        self.assertEqual(
            [(g.lockfile, g.primary) for g in groups],
            [("Cargo.lock", "GHSA-43w2-9j62-hq99"), ("fuzz/Cargo.lock", "GHSA-43w2-9j62-hq99")],
        )
        self.assertEqual(groups[0].sources, ["code-scanning", "dependabot"])
        self.assertIn("CVE-1", groups[0].ids)

    def test_dependabot_alerts(self) -> None:
        raw = [
            {"security_advisory": {"ghsa_id": "GHSA-a"}, "dependency": {"manifest_path": "ui/package-lock.json"}},
            {"security_advisory": {}, "dependency": {"manifest_path": "x"}},
        ]
        self.assertEqual(adv.dependabot_to_alerts(raw), [("GHSA-a", "ui/package-lock.json")])


BUNDLE = adv.Analysis(bundles={"bookclerk|ui/package-lock.json": [("react", "19.0.0")]})


def _analysis(functions: dict[str, list[str]] | None = None, error: str = "", plan: str = "run") -> Any:
    """An analysis whose host binaries contain `functions` (binary name to function paths)."""
    binaries = None
    if functions is not None:
        binaries = {
            f"bookclerk|Cargo.lock|{name}": {adv.function_digest(f) for f in fns} for name, fns in functions.items()
        }
    return adv.Analysis(plan=plan, binaries=binaries, error=error, bundles=BUNDLE.bundles)


class DecisionTest(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repo()
        self.entries = {s.key: s for s in self.repo.config.shipped}

    def tearDown(self) -> None:
        self.repo.close()

    def decide(self, key: str, record: dict[str, Any], analysis: Any = BUNDLE) -> Any:
        entry = self.entries[key]
        return adv.decide(_group(entry.lockfile, record), entry, EVIDENCE, analysis, self.repo.root)

    def test_cargo_verdicts(self) -> None:
        host = "bookclerk|Cargo.lock"
        sdk = "rust-sdk|Cargo.lock"
        with_fn = _record("RUSTSEC-1", "smallvec", "crates.io", "1.6.1", functions=["smallvec::SmallVec::grow"])
        no_fn = _record("RUSTSEC-2", "time", "crates.io", "0.2.0")
        dev = _record("RUSTSEC-3", "devonly", "crates.io", "2.0.0")
        self.assertEqual(self.decide(host, no_fn).verdict, adv.SHIPPED_NO_FUNCTIONS)
        self.assertEqual(self.decide(sdk, no_fn).verdict, adv.SHIPPED_NO_FUNCTIONS)
        self.assertEqual(self.decide(host, dev).verdict, adv.NOT_SHIPPED)
        self.assertEqual(self.decide(sdk, with_fn).verdict, adv.NOT_SHIPPED)
        # Not run yet (deferred to the weekly scan): no verdict, nothing filed.
        self.assertEqual(self.decide(host, with_fn).verdict, adv.AWAITING_CALLS)
        # Ran and failed: fail closed on the closure.
        self.assertEqual(
            self.decide(host, with_fn, _analysis(error="CalledProcessError")).verdict, adv.SHIPPED_UNANALYZED
        )
        decision = self.decide(host, with_fn, _analysis({"bookclerkd": ["smallvec::SmallVec::grow", "x::main"]}))
        self.assertEqual(
            (decision.verdict, decision.functions, decision.binaries),
            (adv.REACHABLE, ["smallvec::SmallVec::grow"], ["bookclerkd"]),
        )
        self.assertEqual(self.decide(host, with_fn, _analysis({"bookclerkd": ["x::main"]})).verdict, adv.NOT_CALLED)
        fixed = _record("RUSTSEC-4", "smallvec", "crates.io", "1.5.0")
        self.assertEqual(self.decide(host, fixed).verdict, adv.NOT_SHIPPED)

    def test_npm_verdicts(self) -> None:
        ui = "bookclerk|ui/package-lock.json"
        self.assertEqual(self.decide(ui, _record("GHSA-r", "react", "npm", "19.0.1")).verdict, adv.IN_BUNDLE)
        self.assertEqual(self.decide(ui, _record("GHSA-t", "tailwind-merge", "npm", "3.0.1")).verdict, adv.TREE_SHAKEN)
        self.assertEqual(self.decide(ui, _record("GHSA-v", "vite", "npm", "8.0.1")).verdict, adv.NOT_SHIPPED)
        sdk = "plugin-sdk-npm|packages/plugin-sdk/package-lock.json"
        self.assertEqual(self.decide(sdk, _record("GHSA-s", "smol-toml", "npm", "1.0.1")).verdict, adv.PRODUCTION_DEP)
        self.assertEqual(self.decide(sdk, _record("GHSA-e", "esbuild", "npm", "0.2.0")).verdict, adv.NOT_SHIPPED)

    def test_unbuilt_bundle_falls_back_to_production(self) -> None:
        record = _record("GHSA-t", "tailwind-merge", "npm", "3.0.1")
        for analysis in (adv.Analysis(), adv.Analysis(bundles={"bookclerk|ui/package-lock.json": None})):
            self.assertEqual(
                self.decide("bookclerk|ui/package-lock.json", record, analysis).verdict, adv.PRODUCTION_DEP
            )


class AnalysisTest(unittest.TestCase):
    def test_plan(self) -> None:
        self.assertEqual(adv.plan_call_analysis([], "schedule", False), "not needed")
        self.assertEqual(adv.plan_call_analysis(["X"], "push", False), "deferred")
        self.assertEqual(adv.plan_call_analysis(["X"], "schedule", False), "run")
        self.assertEqual(adv.plan_call_analysis([], "push", True), "run")

    def test_load_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            calls = Path(tmp) / "calls.json"
            self.assertEqual(adv.load_analysis("run", calls, None).error, "missing output")
            calls.write_text(json.dumps({"error": "TimeoutExpired"}), encoding="utf-8")
            self.assertEqual(adv.load_analysis("run", calls, None).error, "TimeoutExpired")
            calls.write_text(json.dumps({"binaries": {}}), encoding="utf-8")
            self.assertEqual(adv.load_analysis("run", calls, None).error, "missing output")
            calls.write_text(json.dumps({"binaries": {"bookclerk|Cargo.lock|b": ["abc"]}}), encoding="utf-8")
            loaded = adv.load_analysis("run", calls, None)
            self.assertEqual((loaded.error, loaded.binaries), ("", {"bookclerk|Cargo.lock|b": {"abc"}}))
            # Deferred: whatever is on disk, no verdict from it.
            self.assertIsNone(adv.load_analysis("deferred", calls, None).binaries)

    def test_check_functions(self) -> None:
        good = {f"crate::f{i}" for i in range(adv.MIN_FUNCTIONS_PER_BINARY)} | {"bookclerkd::main"}
        adv.check_functions("bookclerkd", good)
        with self.assertRaises(ValueError):
            adv.check_functions("bookclerkd", good - {"bookclerkd::main"})
        with self.assertRaises(ValueError):
            adv.check_functions("bookclerkd", {"bookclerkd::main"})


class SeverityTest(unittest.TestCase):
    def test_alias_severity_and_cwes_survive_the_merge(self) -> None:
        rustsec = _record("RUSTSEC-9", "time", "crates.io", "0.2.0", aliases=["GHSA-9"])
        ghsa = _record(
            "GHSA-9", "time", "crates.io", "0.2.0", database_specific={"severity": "CRITICAL", "cwe_ids": ["CWE-787"]}
        )
        merged = adv.with_alias_meta(rustsec, [ghsa])
        self.assertEqual(adv.severity_fields(merged), {"severity": "critical"})
        self.assertEqual(adv.cwe_ids(merged), ["CWE-787"])
        cvss = dict(ghsa, severity=[{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}])
        self.assertIn("cvss_vector_string", adv.severity_fields(adv.with_alias_meta(rustsec, [cvss])))
        self.assertEqual(adv.severity_fields(rustsec), {"severity": "medium"})
        # Only CVSS 3.1 vectors are sent; an older one falls back to the level.
        old = dict(ghsa, severity=[{"type": "CVSS_V3", "score": "CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}])
        self.assertEqual(adv.severity_fields(adv.with_alias_meta(rustsec, [old])), {"severity": "critical"})
        # Groups merged later keep every record's data.
        groups, _ = adv.group_alerts(
            [("RUSTSEC-9", "Cargo.lock", "code-scanning"), ("GHSA-9", "Cargo.lock", "dependabot")],
            {"RUSTSEC-9": adv.with_alias_meta(rustsec, []), "GHSA-9": adv.with_alias_meta(ghsa, [])},
        )
        self.assertEqual(adv.cwe_ids(groups[0].record), ["CWE-787"])


class FilingTest(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repo()
        self.time = _record(
            "RUSTSEC-2",
            "time",
            "crates.io",
            "0.2.0",
            aliases=["GHSA-time"],
            summary="time is broken",
            severity=[{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"}],
            database_specific={"cwe_ids": ["CWE-400", "bogus"]},
        )
        self.smol = _record("GHSA-smol", "smol-toml", "npm", "1.0.1", database_specific={"severity": "HIGH"})
        self.state: dict[str, Any] = {
            "release_tag": "v0.2.0",
            "groups": [
                {"lockfile": "Cargo.lock", "ids": ["GHSA-time", "RUSTSEC-2"], "record": self.time},
                {"lockfile": "packages/plugin-sdk/package-lock.json", "ids": ["GHSA-smol"], "record": self.smol},
                {"lockfile": "fuzz/Cargo.lock", "ids": ["GHSA-time", "RUSTSEC-2"], "record": self.time},
            ],
            "on_record": [],
        }

    def tearDown(self) -> None:
        self.repo.close()

    def run_file(self, gh: Any) -> tuple[list[Any], list[Any], dict[str, int]]:
        decisions, unshipped = adv.decide_all(self.state, self.repo.config, EVIDENCE, BUNDLE, self.repo.root)
        counts = adv.file_decisions(decisions, gh, self.state, BUNDLE, self.repo.root, "abc", "https://run")
        return decisions, unshipped, counts

    def _as_draft(self, body: dict[str, Any], ghsa: str = "GHSA-host", prefix: str = "") -> dict[str, Any]:
        """Turns a filed body into the draft the next run's fetch lists."""
        return {
            "ghsa_id": ghsa,
            "description": prefix + body["description"],
            "vulnerabilities": body["vulnerabilities"],
        }

    def _second_run(self, draft: dict[str, Any]) -> tuple[list[Any], FakeGitHub]:
        self.state["on_record"] = [list(m) for m in adv.MARKER_RE.findall(draft["description"])]
        self.state["drafts"] = [draft]
        gh = FakeGitHub()
        decisions, _, _ = self.run_file(gh)
        return decisions, gh

    def _host_body(self) -> dict[str, Any]:
        first = FakeGitHub()
        self.run_file(first)
        return next(b for b in first.created if b["vulnerabilities"][0]["package"]["name"] == "bookclerk")

    def test_open_draft_is_not_filed_twice_or_rewritten_when_current(self) -> None:
        decisions, gh = self._second_run(self._as_draft(self._host_body()))
        host = next(d for d in decisions if d.entry.artifact.key == "bookclerk")
        self.assertEqual(host.outcome, "on record")
        self.assertEqual(gh.updated, [])
        self.assertNotIn("bookclerk", [b["vulnerabilities"][0]["package"]["name"] for b in gh.created])

    def test_open_draft_gets_new_evidence_and_keeps_maintainer_edits(self) -> None:
        draft = self._as_draft(self._host_body(), prefix="Maintainer: confirmed reachable via the scan job.\n")
        draft["vulnerabilities"][0]["vulnerable_version_range"] = ">= v0.1.0, <= v0.2.0"
        self.state["groups"][0]["ids"] = ["GHSA-time", "RUSTSEC-2", "CVE-2026-1"]
        decisions, gh = self._second_run(draft)
        host = next(d for d in decisions if d.entry.artifact.key == "bookclerk")
        self.assertEqual(host.outcome, "updated")
        ghsa, changes = gh.updated[0]
        self.assertEqual(ghsa, "GHSA-host")
        self.assertTrue(changes["description"].startswith("<!-- osv-advisory: CVE-2026-1 other/bookclerk -->\n"))
        self.assertIn("Maintainer: confirmed reachable", changes["description"])
        self.assertIn("CVE-2026-1", changes["description"])
        self.assertNotIn("vulnerabilities", changes)  # the maintainer's range stays

    def test_unset_range_is_filled_once_a_release_exists(self) -> None:
        self.state["release_tag"] = None
        draft = self._as_draft(self._host_body())
        self.assertIsNone(draft["vulnerabilities"][0]["vulnerable_version_range"])
        self.state["release_tag"] = "v0.2.0"
        _, gh = self._second_run(draft)
        changes = gh.updated[0][1]
        self.assertEqual(changes["vulnerabilities"][0]["vulnerable_version_range"], "<= v0.2.0")
        self.assertIn("v0.2.0", changes["description"])

    def test_open_draft_no_longer_affected_is_reported(self) -> None:
        draft = self._as_draft(self._host_body())
        self.state["on_record"] = [list(m) for m in adv.MARKER_RE.findall(draft["description"])]
        self.state["drafts"] = [draft]
        decisions, _ = adv.decide_all(self.state, self.repo.config, EVIDENCE, BUNDLE, self.repo.root)
        host = next(d for d in decisions if d.entry.artifact.key == "bookclerk")
        host.verdict = adv.NOT_CALLED
        gh = FakeGitHub()
        adv.file_decisions(decisions, gh, self.state, BUNDLE, self.repo.root, "abc", "https://run")
        self.assertEqual(host.outcome, "open draft, now not affected")
        self.assertEqual(gh.updated, [])

    def test_network_errors_mark_one_draft_failed(self) -> None:
        class Stalling(FakeGitHub):
            def create_advisory(self, body: dict[str, Any]) -> str:
                if not self.created:
                    self.created.append({})
                    raise TimeoutError("read timed out")
                return super().create_advisory(body)

        gh = Stalling()
        _, _, counts = self.run_file(gh)
        self.assertEqual((counts["failed"], counts["filed"]), (1, 2))

    def test_closed_or_published_advisory_is_left_alone(self) -> None:
        body = self._host_body()
        self.state["on_record"] = [list(m) for m in adv.MARKER_RE.findall(body["description"])]
        self.state["drafts"] = []
        gh = FakeGitHub()
        self.run_file(gh)
        self.assertEqual(gh.updated, [])
        self.assertNotIn("bookclerk", [b["vulnerabilities"][0]["package"]["name"] for b in gh.created])

    def test_before_a_release_drafts_propose_no_range(self) -> None:
        self.state["release_tag"] = None
        gh = FakeGitHub()
        _, unshipped, counts = self.run_file(gh)
        self.assertEqual(counts["filed"], 3)  # host + rust SDK + TS SDK
        ranges = {
            b["vulnerabilities"][0]["package"]["name"]: b["vulnerabilities"][0]["vulnerable_version_range"]
            for b in gh.created
        }
        # Nothing is published yet, so no artifact gets a range, not even one
        # whose manifest names a version.
        self.assertEqual(set(ranges.values()), {None})
        for body in gh.created:
            self.assertNotIn("None", body["description"])
        self.assertIn("no published release", gh.created[0]["description"])
        self.assertEqual([g.lockfile for g in unshipped], ["fuzz/Cargo.lock"])

    def test_without_a_token_nothing_is_filed(self) -> None:
        _, _, counts = self.run_file(None)
        self.assertEqual(counts["no token"], 3)
        self.assertEqual(counts["filed"], 0)

    def test_files_one_draft_per_artifact(self) -> None:
        gh = FakeGitHub()
        decisions, unshipped, counts = self.run_file(gh)
        self.assertEqual(counts["filed"], 3)
        by_package = {b["vulnerabilities"][0]["package"]["name"]: b for b in gh.created}
        host = by_package["bookclerk"]
        self.assertEqual(host["vulnerabilities"][0]["vulnerable_version_range"], "<= v0.2.0")
        self.assertEqual(host["cvss_vector_string"], "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N")
        self.assertNotIn("severity", host)
        self.assertEqual(host["cwe_ids"], ["CWE-400"])
        self.assertIn("<!-- osv-advisory: RUSTSEC-2 other/bookclerk -->", host["description"])
        self.assertIn("<!-- osv-advisory: GHSA-time other/bookclerk -->", host["description"])
        self.assertEqual(
            set(adv.MARKER_RE.findall(host["description"])),
            {("RUSTSEC-2", "other/bookclerk"), ("GHSA-time", "other/bookclerk")},
        )
        self.assertIn("reachability is unproven", host["description"])
        self.assertIn("time` 0.1.0 → 0.2.0", host["description"])
        sdk = by_package["@bookclerk/plugin-sdk"]
        self.assertEqual(sdk["vulnerabilities"][0]["package"]["ecosystem"], "npm")
        self.assertEqual(sdk["vulnerabilities"][0]["vulnerable_version_range"], "<= 0.3.0")
        self.assertEqual(sdk["severity"], "high")
        self.assertIn("Function-level reachability analysis does not exist for npm", sdk["description"])
        report = adv.render_report(decisions, unshipped, BUNDLE, "v0.2.0", 0)
        self.assertIn("| GHSA-smol |", report)
        self.assertIn("`fuzz/Cargo.lock` | - | not shipped (lockfile)", report)

    def test_on_record_and_cap(self) -> None:
        self.state["on_record"] = [["RUSTSEC-2", "other/bookclerk"]]
        gh = FakeGitHub()
        _, _, counts = self.run_file(gh)
        self.assertEqual((counts["filed"], counts["on record"]), (2, 1))
        old = adv.MAX_FILED_PER_RUN
        try:
            adv.MAX_FILED_PER_RUN = 1
            self.state["on_record"] = []
            gh = FakeGitHub()
            _, _, counts = self.run_file(gh)
            self.assertEqual((counts["filed"], counts["capped"]), (1, 2))
        finally:
            adv.MAX_FILED_PER_RUN = old

    def test_a_refused_draft_does_not_block_the_rest(self) -> None:
        class Refusing(FakeGitHub):
            def create_advisory(self, body: dict[str, Any]) -> str:
                if body["vulnerabilities"][0]["package"]["name"] == "bookclerk":
                    raise urllib.error.HTTPError("u", 422, "Unprocessable", Message(), None)
                return super().create_advisory(body)

        gh = Refusing()
        _, _, counts = self.run_file(gh)
        self.assertEqual((counts["failed"], counts["filed"]), (1, 2))

    def test_same_artifact_twice_gets_one_draft(self) -> None:
        # The host binaries and the web UI are both `bookclerk`.
        self.state["groups"] = [
            {"lockfile": "Cargo.lock", "ids": ["GHSA-x"], "record": _record("GHSA-x", "time", "crates.io", "0.2.0")},
            {
                "lockfile": "ui/package-lock.json",
                "ids": ["GHSA-x"],
                "record": _record("GHSA-x", "react", "npm", "19.0.1"),
            },
        ]
        gh = FakeGitHub()
        _, _, counts = self.run_file(gh)
        # One for the host (the UI hit is then on record), one for the Rust SDK.
        self.assertEqual((counts["filed"], counts["on record"]), (2, 1))
        names = sorted(b["vulnerabilities"][0]["package"]["name"] for b in gh.created)
        self.assertEqual(names, ["bookclerk", "bookclerk-plugin-sdk"])

    def test_call_candidates(self) -> None:
        record = _record("RUSTSEC-1", "smallvec", "crates.io", "1.6.1", functions=["smallvec::SmallVec::grow"])
        groups = [_group("Cargo.lock", record)]
        config = self.repo.config
        self.assertEqual(adv.call_candidates(groups, config, EVIDENCE, set(), self.repo.root), ["RUSTSEC-1"])
        on_record = {("RUSTSEC-1", "other/bookclerk")}
        self.assertEqual(adv.call_candidates(groups, config, EVIDENCE, on_record, self.repo.root), [])
        # Not in the shipped closure: no build, however often the scan runs.
        unshipped = {**EVIDENCE, "bookclerk|Cargo.lock": {"packages": [["time", "0.1.0"]]}}
        self.assertEqual(adv.call_candidates(groups, config, unshipped, set(), self.repo.root), [])
        plain = [_group("Cargo.lock", _record("RUSTSEC-2", "time", "crates.io", "0.2.0"))]
        self.assertEqual(adv.call_candidates(plain, config, EVIDENCE, set(), self.repo.root), [])

    def test_open_drafts_still_get_call_analysis(self) -> None:
        marker = "<!-- osv-advisory: RUSTSEC-1 other/bookclerk -->"
        state = {"on_record": [["RUSTSEC-1", "other/bookclerk"], ["RUSTSEC-2", "other/bookclerk"]]}
        state["drafts"] = [{"ghsa_id": "GHSA-a", "description": marker}]
        self.assertEqual(adv.settled_markers(state), {("RUSTSEC-2", "other/bookclerk")})

    def test_bundle_needed_only_for_web_ui_advisories(self) -> None:
        smol = _record("GHSA-smol", "smol-toml", "npm", "1.0.1")
        config = self.repo.config
        self.assertFalse(adv.bundle_needed([_group("packages/plugin-sdk/package-lock.json", smol)], config))
        self.assertTrue(adv.bundle_needed([_group("ui/package-lock.json", smol)], config))


class UiImportGuardTest(unittest.TestCase):
    """The bundle check reads source maps; assets imported from packages would bypass them."""

    def test_no_package_asset_imports(self) -> None:
        pattern = re.compile(
            r"""from\s+["']([^"'.@/][^"']*|@[^/"']+/[^"']+)\?(?:url|raw|worker|inline)["']|\.wasm["']"""
        )
        offenders = []
        for path in (ROOT / "ui" / "src").rglob("*.ts*"):
            text = path.read_text(encoding="utf-8")
            if pattern.search(text):
                offenders.append(str(path.relative_to(ROOT)))
        self.assertEqual(offenders, [], "extend bundle_packages to see package assets first")


class WorkflowTest(unittest.TestCase):
    """Tokens and secrets stay out of jobs and steps that run build tooling."""

    def setUp(self) -> None:
        text = (ROOT / ".github" / "workflows" / "osv-advisories.yml").read_text(encoding="utf-8")
        self.workflow = yaml.safe_load(text)

    def test_fetch_token_only_in_the_read_step(self) -> None:
        steps = self.workflow["jobs"]["fetch"]["steps"]
        with_token = [s.get("name") for s in steps if "steps.read.outputs.token" in json.dumps(s)]
        self.assertEqual(with_token, ["Read alerts, releases and advisories on record"])
        mint = next(s for s in steps if s.get("id") == "read")
        self.assertTrue(mint["with"]["skip-token-revoke"])
        self.assertNotIn("write", json.dumps(mint["with"]))
        run = next(s for s in steps if s.get("name") == "Read alerts, releases and advisories on record")["run"]
        self.assertIn("trap revoke EXIT", run)

    def test_analyze_job_has_no_secrets(self) -> None:
        job = json.dumps(self.workflow["jobs"]["analyze"])
        self.assertNotIn("secrets.", job)
        self.assertNotIn("token", job.lower())

    def test_file_job_only_writes_advisories_and_builds_nothing(self) -> None:
        steps = self.workflow["jobs"]["file"]["steps"]
        mint = next(s for s in steps if s.get("id") == "app")
        self.assertNotIn("if", mint)
        self.assertEqual(
            {k: v for k, v in mint["with"].items() if k.startswith("permission-")},
            {"permission-repository-advisories": "write"},
        )
        for step in steps:
            run = step.get("run", "")
            self.assertNotRegex(run, r"\b(cargo|npm|npx|vite)\b", step.get("name"))


if __name__ == "__main__":
    unittest.main()
