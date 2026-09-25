#!/usr/bin/env python3
"""Print Bookclerk's predicted selective-CI plan (dry run).

Usage:
  python3 scripts/ci-plan.py --base <sha> --head <sha> [--format json|summary]
  python3 scripts/ci-plan.py --paths crates/bookclerk-cli/src/main.rs --format summary
  python3 scripts/ci-plan.py --metadata-file meta.json --paths a.rs b.rs

CI uses ``scripts/ci-exec.py resolve``, which wraps this prediction with the
execution mode (shadow / full / selective) and writes the job outputs.
See docs/ci.md and GitHub issue #157.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

_SCRIPTS = Path(__file__).resolve().parent
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))

from ci_plan.plan import plan_from_event, plan_to_json, plan_to_summary  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", help="Git base SHA (exclusive side of triple-dot diff)")
    parser.add_argument("--head", help="Git head SHA")
    parser.add_argument("--format", choices=("json", "summary"), default="json")
    parser.add_argument("--workspace-root", default=None, help="Repository root (default: cwd)")
    parser.add_argument("--metadata-file", help="Use this cargo metadata JSON instead of cargo")
    parser.add_argument("--paths-file", help="Newline-separated changed paths (skips git diff)")
    parser.add_argument("--paths", nargs="*", help="Changed paths (skips git diff)")
    parser.add_argument("--force-full", action="store_true", help="Force the full suite")
    args = parser.parse_args(argv)

    metadata = None
    if args.metadata_file:
        metadata = json.loads(Path(args.metadata_file).read_text(encoding="utf-8"))
    paths = None
    if args.paths_file:
        paths = [
            line.strip()
            for line in Path(args.paths_file).read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
    if args.paths is not None:
        paths = (paths or []) + list(args.paths)

    plan = plan_from_event(
        base=args.base,
        head=args.head,
        workspace_root=args.workspace_root,
        metadata=metadata,
        paths=paths,
        force_full=args.force_full,
    )
    sys.stdout.write(plan_to_json(plan) if args.format == "json" else plan_to_summary(plan))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
