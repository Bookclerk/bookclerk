#!/usr/bin/env python3
"""OSV remediation: version ranges, lockfile parsing, planning, rollback and sealing (offline)."""

from __future__ import annotations

import importlib.util
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    path = ROOT / "scripts" / "osv-remediate.py"
    spec = importlib.util.spec_from_file_location("osv_remediate", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["osv_remediate"] = module
    spec.loader.exec_module(module)
    return module


osv = _load()
V = osv.Version.parse


def _vuln(
    vuln_id: str, name: str, ecosystem: str, *ranges: list[dict[str, str]]
) -> dict[str, object]:
    return {
        "id": vuln_id,
        "affected": [
            {
                "package": {"ecosystem": ecosystem, "name": name},
                "ranges": [{"type": "SEMVER", "events": events} for events in ranges],
            }
        ],
    }


RUSTLS = _vuln(
    "RUSTSEC-2026-0346",
    "rustls",
    "crates.io",
    [{"introduced": "0.23.0"}, {"fixed": "0.23.46"}],
)

CARGO_LOCK = """
version = 4

[[package]]
name = "rustls"
version = "0.21.12"

[[package]]
name = "rustls"
version = "0.23.45"

[[package]]
name = "bookclerk"
version = "0.1.0"
"""


class VersionTest(unittest.TestCase):
    def test_semver_precedence(self) -> None:
        ordered = [
            "0.9.1",
            "0.10.0-alpha",
            "0.10.0-alpha.2",
            "0.10.0-beta",
            "0.10.0",
            "0.10.2",
            "1.0.0",
        ]
        parsed = [V(raw) for raw in ordered]
        self.assertTrue(all(p is not None for p in parsed))
        self.assertEqual(sorted(parsed), parsed)

    def test_build_metadata_ignored_and_bad_input(self) -> None:
        self.assertEqual(V("1.2.3+build.5"), V("1.2.3"))
        self.assertIsNone(V("not-a-version"))


class RangeTest(unittest.TestCase):
    def test_affected_only_inside_range(self) -> None:
        self.assertTrue(osv.is_affected(RUSTLS, "crates.io", "rustls", V("0.23.45")))
        self.assertFalse(osv.is_affected(RUSTLS, "crates.io", "rustls", V("0.23.46")))
        self.assertFalse(osv.is_affected(RUSTLS, "crates.io", "rustls", V("0.21.12")))
        self.assertFalse(osv.is_affected(RUSTLS, "npm", "rustls", V("0.23.45")))

    def test_introduced_zero_and_last_affected(self) -> None:
        vuln = _vuln(
            "X", "pkg", "npm", [{"introduced": "0"}, {"last_affected": "2.0.0"}]
        )
        self.assertTrue(osv.is_affected(vuln, "npm", "pkg", V("2.0.0")))
        self.assertFalse(osv.is_affected(vuln, "npm", "pkg", V("2.0.1")))

    def test_unbounded_range(self) -> None:
        vuln = _vuln("X", "pkg", "npm", [{"introduced": "1.5.0"}])
        self.assertTrue(osv.is_affected(vuln, "npm", "pkg", V("9.0.0")))
        self.assertIsNone(osv.smallest_fix(vuln, "npm", "pkg", V("9.0.0")))

    def test_limit_event_bounds_range(self) -> None:
        vuln = _vuln("X", "pkg", "npm", [{"introduced": "1.0.0"}, {"limit": "2.0.0"}])
        self.assertTrue(osv.is_affected(vuln, "npm", "pkg", V("1.9.9")))
        self.assertFalse(osv.is_affected(vuln, "npm", "pkg", V("2.0.0")))

    def test_smallest_fix_per_branch(self) -> None:
        vuln = _vuln(
            "X",
            "pkg",
            "npm",
            [{"introduced": "1.0.0"}, {"fixed": "1.4.2"}],
            [{"introduced": "2.0.0"}, {"fixed": "2.1.0"}],
        )
        self.assertEqual(osv.smallest_fix(vuln, "npm", "pkg", V("1.4.0")), V("1.4.2"))
        self.assertEqual(osv.smallest_fix(vuln, "npm", "pkg", V("2.0.3")), V("2.1.0"))

    def test_explicit_versions_list(self) -> None:
        vuln = {
            "affected": [
                {"package": {"ecosystem": "npm", "name": "pkg"}, "versions": ["1.0.0"]}
            ]
        }
        self.assertTrue(osv.is_affected(vuln, "npm", "pkg", V("1.0.0")))
        self.assertFalse(osv.is_affected(vuln, "npm", "pkg", V("1.0.1")))


UV_LOCK = """
version = 1
revision = 3
requires-python = ">=3.11"

[[package]]
name = "bookclerk-plugin-sdk"
version = "0.1.0"
source = { editable = "." }

[[package]]
name = "local-helper"
version = "1.0.0"
source = { directory = "../helper" }

[[package]]
name = "forked"
version = "2.0.0"
source = { git = "https://example.invalid/forked?rev=abc#abc" }

[[package]]
name = "pyyaml"
version = "6.0.2"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "typing-extensions"
version = "4.12.2"
source = { registry = "https://pypi.org/simple" }
"""

PYYAML = _vuln("P", "PyYAML", "PyPI", [{"introduced": "0"}, {"fixed": "6.0.3"}])


class LockfileTest(unittest.TestCase):
    def test_cargo_lock_keeps_every_version(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "Cargo.lock"
            lock.write_text(CARGO_LOCK, encoding="utf-8")
            versions = osv.locked_versions(lock)
        self.assertEqual(versions["rustls"], [V("0.21.12"), V("0.23.45")])

    def test_package_lock_nested_and_scoped(self) -> None:
        data = {
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "ui", "version": "0.0.0"},
                "node_modules/vite": {"version": "8.3.4"},
                "node_modules/a/node_modules/@scope/b": {"version": "1.0.0"},
                "node_modules/linked": {"link": True, "resolved": "../x"},
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "package-lock.json"
            lock.write_text(json.dumps(data), encoding="utf-8")
            versions = osv.locked_versions(lock)
        self.assertEqual(versions["vite"], [V("8.3.4")])
        self.assertEqual(versions["@scope/b"], [V("1.0.0")])
        self.assertNotIn("linked", versions)
        self.assertNotIn("", versions)

    def test_uv_lock_normalizes_names_and_skips_the_project(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "uv.lock"
            lock.write_text(UV_LOCK, encoding="utf-8")
            versions = osv.locked_versions(lock)
        self.assertEqual(versions["pyyaml"], [V("6.0.2")])
        self.assertEqual(versions["typing-extensions"], [V("4.12.2")])
        self.assertNotIn("bookclerk-plugin-sdk", versions)
        self.assertNotIn("local-helper", versions)
        self.assertNotIn("forked", versions)

    def test_pypi_advisory_names_match_normalized(self) -> None:
        # Advisories keep the display spelling; uv.lock records PEP 503 names.
        self.assertTrue(osv.is_affected(PYYAML, "PyPI", "pyyaml", V("6.0.2")))
        self.assertFalse(osv.is_affected(PYYAML, "PyPI", "pyyaml", V("6.0.3")))
        # Other ecosystems still compare exactly.
        self.assertFalse(osv.is_affected(RUSTLS, "crates.io", "RusTLS", V("0.23.45")))

    def test_ranges_that_are_not_semver_are_left_alone(self) -> None:
        # Dropping `introduced: 2.0a1` would leave only (0, 1.5) and call 2.1 safe.
        pep440 = _vuln(
            "P2",
            "PyYAML",
            "PyPI",
            [{"introduced": "0"}, {"fixed": "1.5"}, {"introduced": "2.0a1"}, {"fixed": "2.4"}],
        )
        self.assertFalse(osv.ranges_parse(pep440, "PyPI", "pyyaml"))
        self.assertTrue(osv.ranges_parse(PYYAML, "PyPI", "pyyaml"))
        # OSV allows `limit: *` (unbounded).
        unbounded = _vuln("P3", "PyYAML", "PyPI", [{"introduced": "0"}, {"limit": "*"}])
        self.assertTrue(osv.ranges_parse(unbounded, "PyPI", "pyyaml"))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            (root / "uv.lock").write_text(UV_LOCK, encoding="utf-8")
            alert = osv.Alert("P2", "uv.lock")
            targets, unmatched = osv.plan_targets([alert], {"P2": pep440}, root)
        self.assertEqual((targets, unmatched), ([], 1))


class PlanTest(unittest.TestCase):
    def test_alerts_deduplicated_and_incomplete_skipped(self) -> None:
        raw = [
            {
                "rule": {"id": "A"},
                "most_recent_instance": {"location": {"path": "Cargo.lock"}},
            },
            {
                "rule": {"id": "A"},
                "most_recent_instance": {"location": {"path": "Cargo.lock"}},
            },
            {"rule": {"id": "B"}, "most_recent_instance": {}},
            {"rule": {}, "most_recent_instance": {"location": {"path": "Cargo.lock"}}},
        ]
        self.assertEqual(osv.alerts_from_api(raw), [osv.Alert("A", "Cargo.lock")])

    def test_cve_rule_matches_through_alias(self) -> None:
        cve = {
            "id": "CVE-2026-1",
            "aliases": ["RUSTSEC-2026-0346"],
            "affected": [
                {"package": {"ecosystem": "GIT", "name": "github.com/rustls/rustls"}}
            ],
        }
        merged = osv.merge_aliases(cve, [RUSTLS])
        self.assertEqual(merged["id"], "CVE-2026-1")
        self.assertTrue(osv.is_affected(merged, "crates.io", "rustls", V("0.23.45")))
        self.assertFalse(osv.is_affected(cve, "crates.io", "rustls", V("0.23.45")))

    def test_alert_hints_come_from_rule_and_message_text(self) -> None:
        raw = [
            {
                "rule": {
                    "id": "CVE-2026-111943",
                    "full_description": {"text": "unused"},
                    "help": "Also published as: RUSTSEC-2026-0382, "
                    "[RUSTSEC-2026-0381](https://osv.dev/RUSTSEC-2026-0381) "
                    "and CVE-2026-111943",
                },
                "most_recent_instance": {
                    "location": {"path": "Cargo.lock"},
                    "message": {
                        "text": "Package 'webpki-roots@1.0.9' is vulnerable to "
                        "'CVE-2026-111943' (also known as 'RUSTSEC-2026-0381', "
                        "'RUSTSEC-2026-0383', 'GHSA-xxxx-not-an-id')."
                    },
                },
            }
        ]
        [alert] = osv.alerts_from_api(raw)
        self.assertEqual(alert, osv.Alert("CVE-2026-111943", "Cargo.lock"))
        self.assertEqual(
            alert.hints,
            ("RUSTSEC-2026-0382", "RUSTSEC-2026-0381", "RUSTSEC-2026-0383"),
        )

    def test_hints_merge_across_alerts_on_one_advisory(self) -> None:
        alerts = [
            osv.Alert("CVE-1", "Cargo.lock", ("RUSTSEC-2026-0001",)),
            osv.Alert("CVE-1", "fuzz/Cargo.lock", ("RUSTSEC-2026-0002",)),
            osv.Alert("RUSTSEC-2026-0003", "Cargo.lock"),
        ]
        self.assertEqual(
            osv.alert_hints_by_id(alerts),
            {
                "CVE-1": ("RUSTSEC-2026-0001", "RUSTSEC-2026-0002"),
                "RUSTSEC-2026-0003": (),
            },
        )

    def test_unknown_cve_rule_resolves_through_hints(self) -> None:
        roots = _vuln(
            "RUSTSEC-2026-0381",
            "webpki-roots",
            "crates.io",
            [{"introduced": "1.0.0"}, {"fixed": "1.0.10"}],
        )
        roots["aliases"] = ["CVE-2026-111943"]
        webpki = _vuln(
            "RUSTSEC-2026-0382",
            "rustls-webpki",
            "crates.io",
            [{"introduced": "0"}, {"fixed": "0.103.16"}],
        )
        known = {"RUSTSEC-2026-0381": roots, "RUSTSEC-2026-0382": webpki}
        looked_up: list[str] = []

        def fake_fetch(vuln_id: str) -> dict[str, object] | None:
            looked_up.append(vuln_id)
            return known.get(vuln_id)

        original = osv.fetch_vuln
        osv.fetch_vuln = fake_fetch
        try:
            merged = osv.fetch_vuln_with_aliases(
                "CVE-2026-111943", ("RUSTSEC-2026-0381", "RUSTSEC-2026-0382")
            )
            nothing = osv.fetch_vuln_with_aliases("CVE-2026-1", ("CVE-2026-2",))
        finally:
            osv.fetch_vuln = original
        assert merged is not None
        self.assertEqual(merged["id"], "RUSTSEC-2026-0381")
        self.assertIn("CVE-2026-111943", merged["aliases"])
        self.assertIn("RUSTSEC-2026-0382", merged["aliases"])
        self.assertTrue(
            osv.is_affected(merged, "crates.io", "webpki-roots", V("1.0.9"))
        )
        self.assertTrue(
            osv.is_affected(merged, "crates.io", "rustls-webpki", V("0.103.15"))
        )
        self.assertFalse(
            osv.is_affected(merged, "crates.io", "rustls-webpki", V("0.103.16"))
        )
        self.assertIsNone(nothing)
        # Each id is looked up once: the CVE alias is not fetched again.
        self.assertEqual(looked_up.count("CVE-2026-111943"), 1)

    def test_plan_matches_only_affected_versions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            (root / "Cargo.lock").write_text(CARGO_LOCK, encoding="utf-8")
            alerts = [
                osv.Alert("RUSTSEC-2026-0346", "Cargo.lock"),
                osv.Alert("UNKNOWN", "Cargo.lock"),
                osv.Alert("RUSTSEC-2026-0346", "../outside/Cargo.lock"),
                osv.Alert("RUSTSEC-2026-0346", "requirements.txt"),
            ]
            vulns = {"RUSTSEC-2026-0346": RUSTLS, "UNKNOWN": None}
            targets, unmatched = osv.plan_targets(alerts, vulns, root)
        self.assertEqual(unmatched, 3)
        self.assertEqual(len(targets), 1)
        self.assertEqual((targets[0].name, targets[0].locked), ("rustls", V("0.23.45")))
        self.assertEqual(set(targets[0].vulns), {"RUSTSEC-2026-0346"})


FAKE_CARGO = """#!/usr/bin/env python3
import os, sys
from pathlib import Path
args = sys.argv[1:]
lock = Path(args[args.index("--manifest-path") + 1]).parent / "Cargo.lock"
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(" ".join(args) + "\\n")
if "--precise" in args:
    sys.exit(int(os.environ.get("FAKE_PRECISE_EXIT", "0")))
lock.write_text(lock.read_text().replace('"0.23.45"', os.environ["FAKE_UPDATE_TO"]))
sys.exit(int(os.environ.get("FAKE_UPDATE_EXIT", "0")))
"""

# Rewrites package-lock.json in its working directory, then exits FAKE_UPDATE_EXIT.
FAKE_NPM = """#!/usr/bin/env python3
import os, sys
from pathlib import Path
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(" ".join(sys.argv[1:]) + "\\n")
lock = Path("package-lock.json")
lock.write_text(lock.read_text().replace('"1.0.0"', os.environ["FAKE_UPDATE_TO"]))
sys.exit(int(os.environ.get("FAKE_UPDATE_EXIT", "0")))
"""

NPM_LOCK = json.dumps(
    {
        "lockfileVersion": 3,
        "packages": {"": {"name": "ui"}, "node_modules/left-pad": {"version": "1.0.0"}},
    }
)
# Rewrites uv.lock in its working directory; an `==` (exact) upgrade exits
# FAKE_PRECISE_EXIT, a plain upgrade FAKE_UPDATE_EXIT.
FAKE_UV = """#!/usr/bin/env python3
import os, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(" ".join(args) + "\\n")
if "==" in args[-1]:
    sys.exit(int(os.environ.get("FAKE_PRECISE_EXIT", "0")))
lock = Path("uv.lock")
lock.write_text(lock.read_text().replace('"6.0.2"', os.environ["FAKE_UPDATE_TO"]))
sys.exit(int(os.environ.get("FAKE_UPDATE_EXIT", "0")))
"""

LEFT_PAD = _vuln("N", "left-pad", "npm", [{"introduced": "0"}, {"fixed": "1.0.1"}])


class ApplyTest(unittest.TestCase):
    def _run(
        self,
        precise_exit: str,
        update_to: str,
        *,
        update_exit: str = "0",
        npm: bool = False,
        uv: bool = False,
        tools: bool = True,
    ) -> tuple[object, str, list[str]]:
        """Applies one target against a fake package manager on PATH.

        Returns the replacement version, the lockfile afterwards, and the
        argument lines the fake tool logged.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            if tools:
                for tool, script in (
                    ("cargo", FAKE_CARGO),
                    ("npm", FAKE_NPM),
                    ("uv", FAKE_UV),
                ):
                    exe = bin_dir / tool
                    exe.write_text(script, encoding="utf-8")
                    exe.chmod(exe.stat().st_mode | stat.S_IEXEC)
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            (root / "Cargo.lock").write_text(CARGO_LOCK, encoding="utf-8")
            (root / "package-lock.json").write_text(NPM_LOCK, encoding="utf-8")
            (root / "uv.lock").write_text(UV_LOCK, encoding="utf-8")
            log = root / "log"
            log.touch()
            # Without tools, PATH holds only the empty bin dir.
            path = (
                str(bin_dir)
                if not tools
                else f"{bin_dir}{os.pathsep}{os.environ['PATH']}"
            )
            env = {
                "PATH": path,
                "FAKE_LOG": str(log),
                "FAKE_PRECISE_EXIT": precise_exit,
                "FAKE_UPDATE_TO": update_to,
                "FAKE_UPDATE_EXIT": update_exit,
            }
            saved = os.environ.copy()
            os.environ.update(env)
            try:
                if uv:
                    target = osv.Target(
                        "uv.lock", "PyPI", "pyyaml", V("6.0.2"), {"P": PYYAML}
                    )
                elif npm:
                    target = osv.Target(
                        "package-lock.json",
                        "npm",
                        "left-pad",
                        V("1.0.0"),
                        {"N": LEFT_PAD},
                    )
                else:
                    target = osv.Target(
                        "Cargo.lock", "crates.io", "rustls", V("0.23.45"), {"R": RUSTLS}
                    )
                result = osv.apply_target(target, root)
            finally:
                os.environ.clear()
                os.environ.update(saved)
            return (
                result,
                (root / target.lockfile).read_text(encoding="utf-8"),
                log.read_text().splitlines(),
            )

    def test_falls_back_to_compatible_update(self) -> None:
        result, lock, calls = self._run("101", '"0.23.47"')
        self.assertEqual(result, V("0.23.47"))
        self.assertIn('"0.23.47"', lock)
        self.assertEqual(len(calls), 2)
        self.assertIn("--precise 0.23.46", calls[0])

    def test_rolls_back_when_still_affected(self) -> None:
        result, lock, calls = self._run("101", '"0.23.44"')
        self.assertIsNone(result)
        self.assertEqual(lock, CARGO_LOCK)
        self.assertEqual(len(calls), 2)

    def test_failed_update_restores_the_lockfile(self) -> None:
        # Both attempts exit non-zero; the second also rewrote the lockfile.
        result, lock, calls = self._run("101", '"0.23.47"', update_exit="1")
        self.assertIsNone(result)
        self.assertEqual(lock, CARGO_LOCK)
        self.assertEqual(len(calls), 2)

    def test_missing_package_manager_counts_as_failed(self) -> None:
        result, lock, calls = self._run("0", '"0.23.47"', tools=False)
        self.assertIsNone(result)
        self.assertEqual(lock, CARGO_LOCK)
        self.assertEqual(calls, [])

    def test_npm_update_is_lockfile_only_without_scripts(self) -> None:
        result, lock, calls = self._run("0", '"1.0.2"', npm=True)
        self.assertEqual(result, V("1.0.2"))
        self.assertIn('"1.0.2"', lock)
        self.assertEqual(
            calls,
            [
                "update left-pad --package-lock-only --ignore-scripts --no-audit --no-fund"
            ],
        )

    def test_uv_exact_fix_then_plain_upgrade(self) -> None:
        # The fake leaves the lockfile alone on `==`, so the plain upgrade runs.
        result, lock, calls = self._run("0", '"6.0.3"', uv=True)
        self.assertEqual(result, V("6.0.3"))
        self.assertIn('"6.0.3"', lock)
        self.assertEqual(
            calls,
            [
                "lock --no-build --upgrade-package pyyaml==6.0.3",
                "lock --no-build --upgrade-package pyyaml",
            ],
        )

    def test_uv_failure_restores_the_lockfile(self) -> None:
        result, lock, calls = self._run("1", '"6.0.3"', uv=True, update_exit="1")
        self.assertIsNone(result)
        self.assertEqual(lock, UV_LOCK)
        self.assertEqual(len(calls), 2)

    def test_npm_failure_restores_the_lockfile(self) -> None:
        result, lock, _ = self._run("0", '"1.0.2"', npm=True, update_exit="1")
        self.assertIsNone(result)
        self.assertEqual(lock, NPM_LOCK)

    def test_other_affected_line_does_not_block_fix(self) -> None:
        both = _vuln(
            "R",
            "rustls",
            "crates.io",
            [{"introduced": "0.21.0"}, {"fixed": "0.21.13"}],
            [{"introduced": "0.23.0"}, {"fixed": "0.23.46"}],
        )
        target = osv.Target(
            "Cargo.lock", "crates.io", "rustls", V("0.23.45"), {"R": both}
        )
        before = [V("0.21.12"), V("0.23.45")]
        self.assertEqual(
            osv._replacement(target, before, [V("0.21.12"), V("0.23.46")]), V("0.23.46")
        )
        self.assertIsNone(osv._replacement(target, before, before))

    def test_consolidation_onto_existing_fix_accepted(self) -> None:
        target = osv.Target(
            "Cargo.lock", "crates.io", "rustls", V("0.23.45"), {"R": RUSTLS}
        )
        before = [V("0.23.45"), V("0.23.46")]
        self.assertEqual(osv._replacement(target, before, [V("0.23.46")]), V("0.23.46"))
        self.assertIsNone(osv._replacement(target, before, []))

    def test_alert_token_not_passed_to_children(self) -> None:
        os.environ["ALERTS_TOKEN"] = "secret"
        try:
            self.assertNotIn("ALERTS_TOKEN", osv._child_env())
        finally:
            del os.environ["ALERTS_TOKEN"]

    def test_resolved_counts_alerts_not_targets(self) -> None:
        old = osv.Target(
            "Cargo.lock", "crates.io", "rustls", V("0.21.12"), {"A": RUSTLS}
        )
        new = osv.Target(
            "Cargo.lock",
            "crates.io",
            "rustls",
            V("0.23.45"),
            {"A": RUSTLS, "B": RUSTLS},
        )
        alerts = [
            osv.Alert("A", "Cargo.lock"),
            osv.Alert("B", "Cargo.lock"),
            osv.Alert("C", "Cargo.lock"),
        ]
        # A spans both lines and only one was fixed; B's only target was fixed; C matched nothing.
        self.assertEqual(osv.resolved_alerts(alerts, [old, new], [new]), 1)
        self.assertEqual(osv.resolved_alerts(alerts, [old, new], [old, new]), 2)

    def test_summary_lists_only_fixes(self) -> None:
        target = osv.Target(
            "Cargo.lock",
            "crates.io",
            "rustls",
            V("0.23.45"),
            {"RUSTSEC-2026-0346": RUSTLS},
        )
        body = osv.render_summary([(target, V("0.23.46"))])
        self.assertIn(
            "| `Cargo.lock` | `rustls` | 0.23.45 | 0.23.46 | RUSTSEC-2026-0346 |", body
        )


BUNDLE_SCRIPT = ROOT / "scripts" / "osv-remediation-bundle.sh"


def _git(repo: Path, *args: str) -> str:
    """Runs git in `repo` with a fixed identity and returns its stdout."""
    return subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@invalid", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


@unittest.skipUnless(
    shutil.which("openssl") and shutil.which("git"), "needs openssl and git"
)
class BundleTest(unittest.TestCase):
    """The sealed bundle round-trips one commit and refuses a wrong key."""

    def _bundle(self, *args: str, key: str) -> subprocess.CompletedProcess[str]:
        env = {**os.environ, "BUNDLE_KEY": key}
        return subprocess.run(
            ["bash", str(BUNDLE_SCRIPT), *args],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_round_trip_checks_out_the_sealed_commit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            src = base / "src"
            src.mkdir()
            _git(src, "init", "-q")
            (src / "Cargo.lock").write_text("old\n", encoding="utf-8")
            _git(src, "add", ".")
            _git(src, "commit", "-qm", "base")
            clone = base / "clone"
            _git(base, "clone", "-q", str(src), str(clone))
            (src / "Cargo.lock").write_text("new\n", encoding="utf-8")
            _git(src, "commit", "-qam", "bump")
            _git(src, "branch", "osv-remediation")
            sealed_dir = base / "sealed"
            sealed_dir.mkdir()
            _git(
                src,
                "bundle",
                "create",
                "-q",
                str(sealed_dir / "head.bundle"),
                "osv-remediation",
                "^HEAD^",
            )
            (sealed_dir / "summary.md").write_text("body\n", encoding="utf-8")
            sealed = base / "sealed.bin"
            self.assertEqual(
                self._bundle("seal", str(sealed_dir), str(sealed), key="k1").returncode,
                0,
            )
            self.assertNotIn(b"new", sealed.read_bytes())

            opened = base / "opened"
            self.assertEqual(
                self._bundle("open", str(sealed), str(opened), key="k1").returncode, 0
            )
            self.assertEqual(
                (opened / "summary.md").read_text(encoding="utf-8"), "body\n"
            )
            _git(
                clone,
                "fetch",
                "-q",
                str(opened / "head.bundle"),
                "refs/heads/osv-remediation",
            )
            _git(clone, "checkout", "-q", "--detach", "FETCH_HEAD")
            self.assertEqual(
                _git(clone, "rev-parse", "HEAD"), _git(src, "rev-parse", "HEAD")
            )
            self.assertEqual(
                (clone / "Cargo.lock").read_text(encoding="utf-8"), "new\n"
            )

            wrong = self._bundle("open", str(sealed), str(base / "wrong"), key="k2")
            self.assertNotEqual(wrong.returncode, 0)

    def test_empty_key_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            result = self._bundle("seal", tmp, str(Path(tmp) / "x.bin"), key="")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("BUNDLE_KEY is empty", result.stderr)


if __name__ == "__main__":
    unittest.main()
