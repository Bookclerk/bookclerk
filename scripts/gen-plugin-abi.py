#!/usr/bin/env python3
"""Sync ABI projections from crates/bookclerk-plugin-abi/schema/abi.json.

- Ensures packages/plugin-sdk/src/generated.ts lists every schema method
- Rewrites packages/plugin-sdk-python/.../abi.py METHOD_NAMES (--write)
- Ensures plugin-toml.json copies match between abi and manifest crates
- Ensures methods.rs METHOD_NAMES match abi.json methods keys (--check)

Exit 1 on drift when --check (default).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ABI = ROOT / "crates/bookclerk-plugin-abi/schema/abi.json"
METHODS_RS = ROOT / "crates/bookclerk-plugin-abi/src/methods.rs"
TS_GENERATED = ROOT / "packages/plugin-sdk/src/generated.ts"
PY_ABI = ROOT / "packages/plugin-sdk-python/src/bookclerk_plugin_sdk/abi.py"
PLUGIN_TOML_SCHEMA = ROOT / "crates/bookclerk-plugin-abi/schema/plugin-toml.json"
MANIFEST_SCHEMA = ROOT / "crates/bookclerk-plugin-manifest/schema/plugin-toml.json"


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


def render_py(names: list[str]) -> str:
    body = ",\n".join(f'    "{n}"' for n in names)
    return f'''"""ABI constants — keep aligned with crates/bookclerk-plugin-abi/schema/abi.json."""

from __future__ import annotations

API_VERSION: int = 1

METHOD_NAMES: tuple[str, ...] = (
{body},
)
'''


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

    py_expected = render_py(names)
    py_current = PY_ABI.read_text(encoding="utf-8") if PY_ABI.is_file() else ""
    if py_current != py_expected:
        if args.write:
            PY_ABI.write_text(py_expected, encoding="utf-8")
            print(f"wrote {PY_ABI}")
        else:
            print("python abi.py METHOD_NAMES drift", file=sys.stderr)
            drift = True

    if drift and args.check and not args.write:
        return 1
    print(f"ok methods={len(names)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
