#!/usr/bin/env python3
"""CLI for Bookclerk dependency-aware CI planning.

Usage:
  python3 scripts/ci-plan.py --base <sha> --head <sha> [--format json|summary|github-output]
  python3 scripts/ci-plan.py --paths-file paths.txt --format summary
  python3 scripts/ci-plan.py --metadata-file meta.json --paths a.rs b.rs

See GitHub issue #157.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

# Allow running as ``python3 scripts/ci-plan.py`` without installing a package.
_SCRIPTS = Path(__file__).resolve().parent
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))

from ci_plan.plan import (  # noqa: E402
    PlanError,
    plan_from_event,
    plan_to_github_output,
    plan_to_json,
    plan_to_summary,
)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", help="Git base SHA (exclusive side of triple-dot diff)")
    parser.add_argument("--head", help="Git head SHA")
    parser.add_argument(
        "--format",
        choices=("json", "summary", "github-output"),
        default="json",
        help="Output format (default: json)",
    )
    parser.add_argument(
        "--workspace-root",
        default=None,
        help="Repository root (default: cwd)",
    )
    parser.add_argument(
        "--metadata-file",
        help="Use this cargo metadata JSON instead of invoking cargo",
    )
    parser.add_argument(
        "--paths-file",
        help="Newline-separated changed paths (skips git diff)",
    )
    parser.add_argument(
        "--paths",
        nargs="*",
        help="Changed paths (skips git diff when provided with --paths-file or alone)",
    )
    parser.add_argument(
        "--force-full",
        action="store_true",
        help="Force full_suite regardless of paths",
    )
    parser.add_argument(
        "--write-summary",
        action="store_true",
        help="Append markdown summary to $GITHUB_STEP_SUMMARY when set",
    )
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
    if args.paths is not None and len(args.paths) > 0:
        paths = list(args.paths) if paths is None else paths + list(args.paths)

    # Explicit empty --paths means empty change set.
    if args.paths is not None and len(args.paths) == 0 and paths is None:
        paths = []

    try:
        plan = plan_from_event(
            base=args.base,
            head=args.head,
            workspace_root=args.workspace_root,
            metadata=metadata,
            paths=paths,
            force_full=args.force_full,
        )
    except PlanError as exc:
        print(f"ci-plan: {exc}", file=sys.stderr)
        return 2

    if args.format == "json":
        sys.stdout.write(plan_to_json(plan))
    elif args.format == "summary":
        sys.stdout.write(plan_to_summary(plan))
    else:
        text = plan_to_github_output(plan)
        sys.stdout.write(text)
        gh_out = os.environ.get("GITHUB_OUTPUT")
        if gh_out:
            with open(gh_out, "a", encoding="utf-8") as fh:
                fh.write(text)

    if args.write_summary:
        summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
        body = plan_to_summary(plan)
        if summary_path:
            with open(summary_path, "a", encoding="utf-8") as fh:
                fh.write(body)
                if not body.endswith("\n"):
                    fh.write("\n")
        else:
            # Local dry-run: still emit summary to stderr when requested.
            sys.stderr.write(body)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
