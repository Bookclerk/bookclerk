#!/usr/bin/env python3
"""Tests for scripts/check-db-plugin-isolation.py."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-db-plugin-isolation.py"


def _load_isolation_module():
    spec = importlib.util.spec_from_file_location("check_db_plugin_isolation", SCRIPT)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class DbPluginIsolationTests(unittest.TestCase):
    def test_script_passes_on_tree(self) -> None:
        proc = subprocess.run(
            [sys.executable, str(SCRIPT)],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr or proc.stdout)

    def test_library_production_forbids_physical_engine_and_host_sql(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("LIBRARY_PHYSICAL_ENGINE", text)
        self.assertIn("LIBRARY_BACKEND_IDENT", text)
        self.assertNotIn("LIBRARY_BACKEND_ALLOW", text)
        self.assertNotIn("host_sql.rs", text)
        src = ROOT / "crates" / "bookclerk-library" / "src"
        self.assertFalse(
            (src / "host_sql.rs").exists(),
            "canonical SeaORM transport must not live in bookclerk-library",
        )

    def test_unprefixed_bookkeeping_matches_case_variants(self) -> None:
        mod = _load_isolation_module()
        patterns = mod.FORBIDDEN_UNPREFIXED_BOOKKEEPING
        self.assertTrue(
            any(rx.search('CREATE TABLE SCHEMA_MIGRATIONS (id INT)') for rx in patterns),
            "uppercase SCHEMA_MIGRATIONS must match",
        )
        self.assertTrue(
            any(rx.search('let t = "Plugin_Migrations";') for rx in patterns),
            "mixed-case plugin_migrations must match",
        )
        self.assertIsNone(
            patterns[0].search("caps.schema_migrations = true"),
            "DbCapabilities field assign must stay excluded",
        )
        self.assertIsNone(
            patterns[0].search("bookclerk_schema_migrations"),
            "prefixed bookkeeping must not match",
        )


if __name__ == "__main__":
    unittest.main()
