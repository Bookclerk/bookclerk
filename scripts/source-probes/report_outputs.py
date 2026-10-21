#!/usr/bin/env python3
"""Print GitHub Actions step outputs summarizing a `probe_all.py` report.

Writes `failed`, `auth_failed` and `auth_ran` as `key=true|false` lines on
stdout (append them to `$GITHUB_OUTPUT`). Auth checks that were skipped for
missing secrets count neither as failed nor as run; a missing report counts
as a public failure, as does an unreadable or malformed one.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def outputs(report_path: Path) -> dict[str, bool]:
    """Summarize one report into the three step outputs."""
    unusable = {"failed": True, "auth_failed": False, "auth_ran": False}
    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return unusable
    if not isinstance(report, dict):
        return unusable
    checks = report.get("checks") or []
    ran = [c for c in checks if not c.get("skipped")]
    return {
        "failed": any(not c.get("ok") and not c.get("needs_auth") for c in ran),
        "auth_failed": any(not c.get("ok") and c.get("needs_auth") for c in ran),
        "auth_ran": any(c.get("needs_auth") for c in ran),
    }


def main(argv: list[str]) -> int:
    """CLI entry point: `report_outputs.py <report.json>`."""
    if len(argv) != 2:
        print("usage: report_outputs.py <report.json>", file=sys.stderr)
        return 2
    for key, value in outputs(Path(argv[1])).items():
        print(f"{key}={'true' if value else 'false'}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
