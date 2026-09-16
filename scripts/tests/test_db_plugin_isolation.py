#!/usr/bin/env python3
"""Tests for scripts/check-db-plugin-isolation.py."""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-db-plugin-isolation.py"


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


if __name__ == "__main__":
    unittest.main()
