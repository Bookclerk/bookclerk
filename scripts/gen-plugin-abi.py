#!/usr/bin/env python3
"""Sync ABI projections from crates/bookclerk-plugin-abi/schema/abi.json.

- Ensures packages/plugin-sdk/src/generated.ts lists every schema method
- Ensures packages/plugin-sdk-python/.../abi.py ``METHOD_NAMES`` match schema
  (preserves Google-style docs / TypedDicts; ``--write`` rewrites only that
  tuple, or scaffolds a stub file when missing)
- Ensures plugin-toml.json copies match between abi and manifest crates
- Ensures methods.rs METHOD_NAMES match abi.json methods keys (--check)
- Ensures fixtures/wire/*.json object keys are camelCase (no `_` in keys)

Exit 1 on drift when --check (default).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
ABI = ROOT / "crates/bookclerk-plugin-abi/schema/abi.json"
METHODS_RS = ROOT / "crates/bookclerk-plugin-abi/src/methods.rs"
TS_GENERATED = ROOT / "packages/plugin-sdk/src/generated.ts"
PY_ABI = ROOT / "packages/plugin-sdk-python/src/bookclerk_plugin_sdk/abi.py"
PLUGIN_TOML_SCHEMA = ROOT / "crates/bookclerk-plugin-abi/schema/plugin-toml.json"
MANIFEST_SCHEMA = ROOT / "crates/bookclerk-plugin-manifest/schema/plugin-toml.json"
WIRE_FIXTURES = ROOT / "crates/bookclerk-plugin-abi/fixtures/wire"

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

# Matches `METHOD_NAMES: tuple[str, ...] = ( ... )` including a trailing comma.
METHOD_NAMES_RE = re.compile(
    r"METHOD_NAMES:\s*tuple\[str,\s*\.\.\.\]\s*=\s*\((.*?)\)\s*\n",
    re.DOTALL,
)


def method_names_from_schema() -> list[str]:
    schema = json.loads(ABI.read_text(encoding="utf-8"))
    methods = schema["properties"]["methods"]["properties"]
    return list(methods.keys())


def method_names_from_methods_rs() -> list[str]:
    """Parse `pub const NAME: &str = "..."` entries in methods.rs modules."""
    text = METHODS_RS.read_text(encoding="utf-8")
    # Prefer METHOD_NAMES array expansion via individual NAME constants.
    names = re.findall(r'pub const NAME: &str = "([^"]+)";', text)
    if not names:
        raise SystemExit(f"no NAME constants found in {METHODS_RS}")
    return names


def method_names_tuple_body(names: list[str]) -> str:
    body = ",\n".join(f'    "{n}"' for n in names)
    return f"(\n{body},\n)"


def method_names_from_py(text: str) -> list[str] | None:
    """Return METHOD_NAMES entries from abi.py, or None if the binding is absent."""
    match = METHOD_NAMES_RE.search(text)
    if not match:
        return None
    return re.findall(r'"([^"]+)"', match.group(1))


def render_py_stub(names: list[str]) -> str:
    """Minimal abi.py used only when the file is missing."""
    return f'''"""ABI constants — keep aligned with crates/bookclerk-plugin-abi/schema/abi.json."""

from __future__ import annotations

API_VERSION: int = 1

METHOD_NAMES: tuple[str, ...] = {method_names_tuple_body(names)}
'''


def sync_py_method_names(text: str, names: list[str]) -> str:
    """Replace the METHOD_NAMES tuple in place; preserve surrounding docs/types."""
    replacement = f"METHOD_NAMES: tuple[str, ...] = {method_names_tuple_body(names)}\n"
    if not METHOD_NAMES_RE.search(text):
        raise SystemExit(f"METHOD_NAMES binding not found in {PY_ABI}")
    return METHOD_NAMES_RE.sub(replacement, text, count=1)


def collect_snake_keys(value: Any, path: str = "$") -> list[str]:
    """Return paths of object keys that contain `_` (not camelCase)."""
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


def check_wire_fixtures() -> list[str]:
    """Validate golden wire fixtures exist and use camelCase object keys."""
    errors: list[str] = []
    if not WIRE_FIXTURES.is_dir():
        return [f"missing wire fixtures dir: {WIRE_FIXTURES}"]
    present = {p.name for p in WIRE_FIXTURES.glob("*.json")}
    for name in REQUIRED_WIRE_FIXTURES:
        if name not in present:
            errors.append(f"missing required wire fixture: {name}")
    for path in sorted(WIRE_FIXTURES.glob("*.json")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            errors.append(f"{path.name}: invalid JSON ({exc})")
            continue
        for bad in collect_snake_keys(data):
            errors.append(f"{path.name}: non-camelCase key at {bad}")
        # Spot-check a few multi-word fields against abi.json $defs naming.
        if path.name == "login.request.json" and "pluginDataDir" not in data:
            errors.append(f"{path.name}: expected pluginDataDir")
        if path.name == "dbExecute.result.json" and "lastInsertId" not in data:
            errors.append(f"{path.name}: expected lastInsertId")
        if path.name == "put.s3.request.json" and "forcePathStyle" not in data:
            errors.append(f"{path.name}: expected forcePathStyle")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if not args.write:
        args.check = True

    names = method_names_from_schema()
    rs_names = method_names_from_methods_rs()
    drift = False

    if names != rs_names:
        print(
            "abi.json methods keys ≠ methods.rs NAME constants:\n"
            f"  schema only: {sorted(set(names) - set(rs_names))}\n"
            f"  methods.rs only: {sorted(set(rs_names) - set(names))}\n"
            f"  order mismatch: {names != rs_names}",
            file=sys.stderr,
        )
        if set(names) != set(rs_names) or names != rs_names:
            drift = True

    if MANIFEST_SCHEMA.read_text(encoding="utf-8") != PLUGIN_TOML_SCHEMA.read_text(
        encoding="utf-8"
    ):
        print("plugin-toml.json differs between abi and manifest crates", file=sys.stderr)
        drift = True
        if args.write:
            PLUGIN_TOML_SCHEMA.write_text(
                MANIFEST_SCHEMA.read_text(encoding="utf-8"), encoding="utf-8"
            )
            print(f"synced {PLUGIN_TOML_SCHEMA}")
            drift = False

    ts = TS_GENERATED.read_text(encoding="utf-8")
    missing = [n for n in names if f'"{n}"' not in ts]
    if missing:
        print(f"generated.ts missing methods: {missing}", file=sys.stderr)
        drift = True

    py_current = PY_ABI.read_text(encoding="utf-8") if PY_ABI.is_file() else ""
    py_names = method_names_from_py(py_current) if py_current else None
    if py_names != names:
        if args.write:
            if not py_current:
                PY_ABI.write_text(render_py_stub(names), encoding="utf-8")
            else:
                PY_ABI.write_text(
                    sync_py_method_names(py_current, names), encoding="utf-8"
                )
            print(f"wrote METHOD_NAMES in {PY_ABI}")
        else:
            detail = (
                "missing METHOD_NAMES binding"
                if py_names is None
                else (
                    f"schema only: {sorted(set(names) - set(py_names))}; "
                    f"abi.py only: {sorted(set(py_names) - set(names))}; "
                    f"order mismatch: {py_names != names}"
                )
            )
            print(f"python abi.py METHOD_NAMES drift ({detail})", file=sys.stderr)
            drift = True

    # Wire fixtures are not auto-fixable; fail even under `--write`.
    wire_errors = check_wire_fixtures()
    if wire_errors:
        for err in wire_errors:
            print(f"wire fixtures: {err}", file=sys.stderr)

    if wire_errors:
        return 1
    if drift and args.check and not args.write:
        return 1
    print(f"ok methods={len(names)} wire_fixtures={len(REQUIRED_WIRE_FIXTURES)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
