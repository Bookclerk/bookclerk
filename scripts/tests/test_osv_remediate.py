#!/usr/bin/env python3
"""OSV remediation: version ranges, lockfile parsing, planning and rollback (offline)."""

from __future__ import annotations

import importlib.util
import json
import os
import stat
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
with open(os.environ["FAKE_CARGO_LOG"], "a") as log:
    log.write(" ".join(args) + "\\n")
if "--precise" in args:
    sys.exit(int(os.environ.get("FAKE_PRECISE_EXIT", "0")))
lock.write_text(lock.read_text().replace('"0.23.45"', os.environ["FAKE_UPDATE_TO"]))
"""


class ApplyTest(unittest.TestCase):
    def _run(self, precise_exit: str, update_to: str) -> tuple[object, str, list[str]]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            cargo = bin_dir / "cargo"
            cargo.write_text(FAKE_CARGO, encoding="utf-8")
            cargo.chmod(cargo.stat().st_mode | stat.S_IEXEC)
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            (root / "Cargo.lock").write_text(CARGO_LOCK, encoding="utf-8")
            log = root / "log"
            env = {
                "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                "FAKE_CARGO_LOG": str(log),
                "FAKE_PRECISE_EXIT": precise_exit,
                "FAKE_UPDATE_TO": update_to,
            }
            saved = os.environ.copy()
            os.environ.update(env)
            try:
                target = osv.Target(
                    "Cargo.lock", "crates.io", "rustls", V("0.23.45"), {"R": RUSTLS}
                )
                result = osv.apply_target(target, root)
            finally:
                os.environ.clear()
                os.environ.update(saved)
            return (
                result,
                (root / "Cargo.lock").read_text(encoding="utf-8"),
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

    def test_alert_token_not_passed_to_children(self) -> None:
        os.environ["ALERTS_TOKEN"] = "secret"
        try:
            self.assertNotIn("ALERTS_TOKEN", osv._child_env())
        finally:
            del os.environ["ALERTS_TOKEN"]

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


if __name__ == "__main__":
    unittest.main()
