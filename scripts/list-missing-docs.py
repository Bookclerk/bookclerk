#!/usr/bin/env python3
"""List rustc/rustdoc missing_docs warnings for one or more workspace crates.

Usage:
  python3 scripts/list-missing-docs.py bookclerk-plugin-sdk
  python3 scripts/list-missing-docs.py --all
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import defaultdict


WARNING_RE = re.compile(
    r"warning: missing documentation for ([^\n]+)\n\s+--> ([^:]+):(\d+):",
    re.MULTILINE,
)


def lib_crate_names() -> list[str]:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            text=True,
        )
    )
    names: list[str] = []
    for p in meta["packages"]:
        for t in p.get("targets", []):
            if "lib" in t.get("kind", []):
                names.append(p["name"])
                break
    return sorted(names)


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def missing_for(crate: str) -> list[tuple[str, str, int]]:
    env = {
        **os.environ,
        "RUSTDOCFLAGS": "-W missing_docs",
        "CARGO_TERM_COLOR": "never",
        "TERM": "dumb",
    }
    proc = subprocess.run(
        ["cargo", "rustdoc", "-p", crate, "--all-features", "--color", "never"],
        env=env,
        capture_output=True,
        text=True,
    )
    text = ANSI_RE.sub("", proc.stderr + proc.stdout)
    return [(kind.strip(), path, int(line)) for kind, path, line in WARNING_RE.findall(text)]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("crates", nargs="*", help="Workspace package names")
    parser.add_argument("--all", action="store_true", help="All library crates")
    args = parser.parse_args()
    crates = lib_crate_names() if args.all else args.crates
    if not crates:
        parser.error("pass crate names or --all")

    grand = 0
    by_file: dict[str, list[tuple[str, int]]] = defaultdict(list)
    for crate in crates:
        items = missing_for(crate)
        print(f"=== {crate}: {len(items)} ===", flush=True)
        grand += len(items)
        for kind, path, line in items:
            print(f"  {path}:{line}: {kind}")
            by_file[path].append((kind, line))
    print(f"TOTAL {grand}", flush=True)
    return 1 if grand else 0


if __name__ == "__main__":
    sys.exit(main())
