#!/usr/bin/env python3
"""Apply APK-probe suggested constants onto crates/libation-libro/src/client.rs.

Reads artifacts/librofm-apk-probe/report.json (or --report) and rewrites the
tracked `pub const ...: &str = "...";` lines. Does not touch CLIENT_ID.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

CONST_LINE_RE = re.compile(
    r'^(pub\s+const\s+(?P<name>DEFAULT_BASE_URL|OAUTH_TOKEN_PATH|LIBRARY_PATH|'
    r'DOWNLOAD_MANIFEST_PATH|PACKAGED_M4B_PATH|APP_VER|USER_AGENT_VALUE)\s*:\s*&str\s*=\s*)'
    r'"(?P<value>[^"]*)"(\s*;)',
    re.MULTILINE,
)


def suggested_from_report(report: dict) -> dict[str, str]:
    apk = report["apk"]
    tracked = apk.get("absolute_tracked_paths") or {}
    out: dict[str, str] = {}
    if apk.get("base_url"):
        out["DEFAULT_BASE_URL"] = apk["base_url"]
    if apk.get("oauth_token_path"):
        out["OAUTH_TOKEN_PATH"] = apk["oauth_token_path"]
    if tracked.get("library"):
        out["LIBRARY_PATH"] = tracked["library"]
    if tracked.get("download-manifest"):
        out["DOWNLOAD_MANIFEST_PATH"] = tracked["download-manifest"]
    if tracked.get("audiobooks/{isbn}/packaged_m4b"):
        out["PACKAGED_M4B_PATH"] = tracked["audiobooks/{isbn}/packaged_m4b"]
    if apk.get("version_name"):
        out["APP_VER"] = apk["version_name"]
    if apk.get("okhttp_user_agent"):
        out["USER_AGENT_VALUE"] = apk["okhttp_user_agent"]
    return out


def apply(client_rs: Path, updates: dict[str, str]) -> list[str]:
    text = client_rs.read_text(encoding="utf-8")
    changed: list[str] = []

    def repl(match: re.Match[str]) -> str:
        name = match.group("name")
        old = match.group("value")
        if name not in updates or updates[name] == old:
            return match.group(0)
        changed.append(f"{name}: {old!r} -> {updates[name]!r}")
        # group(1) is the `pub const NAME: &str = ` prefix (includes named name group).
        return f'{match.group(1)}"{updates[name]}";'

    new_text, n = CONST_LINE_RE.subn(repl, text)
    if n == 0:
        raise RuntimeError("no tracked const lines matched in client.rs")
    if changed:
        client_rs.write_text(new_text, encoding="utf-8")
    return changed


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
    )
    parser.add_argument("--report", type=Path, default=None)
    args = parser.parse_args(argv)

    report_path = args.report or (
        args.repo_root / "artifacts/librofm-apk-probe/report.json"
    )
    client_rs = args.repo_root / "crates/libation-libro/src/client.rs"
    if not report_path.exists():
        print(f"error: missing report {report_path}", file=sys.stderr)
        return 2
    if not client_rs.exists():
        print(f"error: missing {client_rs}", file=sys.stderr)
        return 2

    report = json.loads(report_path.read_text(encoding="utf-8"))
    updates = suggested_from_report(report)
    suggested_path = report_path.parent / "suggested_constants.json"
    suggested_path.write_text(json.dumps(updates, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"suggested": updates}, indent=2))

    changed = apply(client_rs, updates)
    if not changed:
        print("No client.rs changes needed.")
        return 0
    print("Updated client.rs:")
    for line in changed:
        print(f"  - {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
