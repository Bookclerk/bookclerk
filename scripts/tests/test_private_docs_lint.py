#!/usr/bin/env python3
"""Regression: every workspace package inherits workspace lints.

``clippy::missing_docs_in_private_items`` is set on ``[workspace.lints.clippy]``.
Workspace lint inheritance is opt-in per package (``[lints] workspace = true``),
so a crate that omits that table silently drops the private-docs contract.
This check reads ``cargo metadata`` and each package manifest — it does not
use a throwaway fixture.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path


def cargo_metadata(root: Path) -> dict:
    proc = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise SystemExit("cargo metadata failed")
    return json.loads(proc.stdout)


def inherits_workspace_lints(manifest: Path) -> bool:
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    lints = data.get("lints")
    return isinstance(lints, dict) and lints.get("workspace") is True


def workspace_declares_private_docs(root: Path) -> bool:
    data = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    clippy = data.get("workspace", {}).get("lints", {}).get("clippy", {})
    return clippy.get("missing_docs_in_private_items") == "warn"


def main() -> int:
    root = Path(__file__).resolve().parents[2]
    if not workspace_declares_private_docs(root):
        sys.stderr.write(
            "root Cargo.toml must set "
            "[workspace.lints.clippy] missing_docs_in_private_items = \"warn\"\n"
        )
        return 1

    meta = cargo_metadata(root)
    workspace_root = Path(meta["workspace_root"])
    missing: list[str] = []
    for pkg in meta["packages"]:
        manifest = Path(pkg["manifest_path"])
        try:
            manifest.relative_to(workspace_root)
        except ValueError:
            continue
        if not inherits_workspace_lints(manifest):
            missing.append(f"{pkg['name']} ({manifest})")

    if missing:
        sys.stderr.write(
            "workspace packages missing `[lints] workspace = true` "
            "(private-docs lint would not apply):\n"
        )
        for name in missing:
            sys.stderr.write(f"  {name}\n")
        return 1

    print(f"ok: {len(meta['packages'])} workspace packages inherit workspace lints")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
