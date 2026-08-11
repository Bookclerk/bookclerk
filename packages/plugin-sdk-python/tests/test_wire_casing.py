"""CamelCase ABI wire fixture checks (Sub-issue A / #130)."""

from __future__ import annotations

import json
import unittest
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[3]
WIRE = ROOT / "crates/bookclerk-plugin-abi/fixtures/wire"

# Keep in sync with scripts/gen-plugin-abi.py REQUIRED_WIRE_FIXTURES.
REQUIRED_WIRE_FIXTURES = (
    "login.request.json",
    "login.result.json",
    "scan.request.json",
    "scan.result.json",
    "fetchTitle.request.json",
    "put.s3.request.json",
    "dbConnect.sqlite.json",
    "dbExecute.result.json",
)


def collect_snake_keys(value: Any, path: str = "$") -> list[str]:
    bad: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            here = f"{path}.{key}"
            if "_" in key:
                bad.append(here)
            bad.extend(collect_snake_keys(child, here))
    elif isinstance(value, list):
        for i, child in enumerate(value):
            bad.extend(collect_snake_keys(child, f"{path}[{i}]"))
    return bad


class WireFixtureCasing(unittest.TestCase):
    def test_required_fixture_set_exists(self) -> None:
        self.assertTrue(WIRE.is_dir(), f"missing {WIRE}")
        present = {p.name for p in WIRE.glob("*.json")}
        for name in REQUIRED_WIRE_FIXTURES:
            self.assertIn(name, present, f"missing required golden fixture: {name}")

    def test_fixtures_are_camel_case(self) -> None:
        self.assertTrue(WIRE.is_dir(), f"missing {WIRE}")
        for path in sorted(WIRE.glob("*.json")):
            data = json.loads(path.read_text(encoding="utf-8"))
            bad = collect_snake_keys(data)
            self.assertEqual(bad, [], f"{path.name} has non-camelCase keys: {bad}")

    def test_login_request_has_plugin_data_dir_camel(self) -> None:
        data = json.loads((WIRE / "login.request.json").read_text(encoding="utf-8"))
        self.assertIn("pluginDataDir", data)
        self.assertNotIn("plugin_data_dir", data)

    def test_db_execute_result_keys(self) -> None:
        data = json.loads((WIRE / "dbExecute.result.json").read_text(encoding="utf-8"))
        self.assertIn("lastInsertId", data)
        self.assertIn("rowsAffected", data)

    def test_put_s3_force_path_style_camel(self) -> None:
        data = json.loads((WIRE / "put.s3.request.json").read_text(encoding="utf-8"))
        self.assertIn("forcePathStyle", data)
        self.assertNotIn("force_path_style", data)


if __name__ == "__main__":
    unittest.main()
