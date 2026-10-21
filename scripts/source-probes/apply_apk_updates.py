#!/usr/bin/env python3
"""Write APK-probe suggestions back into a source's Rust client.

Reads the `suggested` map from `extract_apk_api.py`'s report and rewrites the
matching `const NAME: &str = "...";` lines in the source's `client.rs`. The
probe contract in `check_client_constants.py` names the same values as
literal needles, so each replaced value is also replaced there wherever it
appears as a whole quoted string, and in backticks in the client's doc
comments. `probe_all.py` reads the constants from
`client.rs`, so it follows without edits.

Exit codes:
  0 — applied (or nothing to change)
  2 — missing report / client, or a suggested constant not found
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
from extract_apk_api import SPECS  # noqa: E402

CONTRACT_PY = ROOT / "scripts" / "source-probes" / "check_client_constants.py"


def rewrite_const(text: str, name: str, value: str) -> tuple[str, str | None]:
    """Replace the value of one `&str` const declaration.

    # Returns
    The new text and the old value, or `(text, None)` when the const is absent.
    """
    pattern = re.compile(
        r'^(?P<head>\s*(?:pub(?:\([^)]*\))?\s+)?const\s+'
        + re.escape(name)
        + r'\s*:\s*&str\s*=\s*)"(?P<value>[^"]*)"(?P<tail>\s*;)',
        re.MULTILINE,
    )
    match = pattern.search(text)
    if match is None:
        return text, None
    old = match.group("value")
    new_line = f'{match.group("head")}"{value}"{match.group("tail")}'
    return text[: match.start()] + new_line + text[match.end() :], old


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; see the module docstring for exit codes."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", required=True, choices=sorted(SPECS))
    parser.add_argument("--report", type=Path, default=None, help="Default: artifacts/<source>-apk-probe/report.json")
    args = parser.parse_args(argv)

    spec = SPECS[args.source]
    report_path = args.report or ROOT / "artifacts" / f"{args.source}-apk-probe" / "report.json"
    if not report_path.exists():
        print(f"error: missing report {report_path}", file=sys.stderr)
        return 2
    suggested: dict[str, str] = json.loads(report_path.read_text(encoding="utf-8")).get("suggested") or {}
    unknown = sorted(set(suggested) - set(spec.tracked))
    if unknown:
        print(f"error: report suggests untracked constants: {unknown}", file=sys.stderr)
        return 2

    client_text = spec.client_rs.read_text(encoding="utf-8")
    contract_text = CONTRACT_PY.read_text(encoding="utf-8")
    changed: list[str] = []
    for name, value in sorted(suggested.items()):
        if '"' in value or "\\" in value or "\n" in value:
            print(f"error: refusing to write {name} with quote/backslash/newline: {value!r}", file=sys.stderr)
            return 2
        client_text, old = rewrite_const(client_text, name, value)
        if old is None:
            print(f"error: const {name} not found in {spec.client_rs}", file=sys.stderr)
            return 2
        if old == value:
            continue
        changed.append(f"{name}: {old!r} -> {value!r}")
        # Doc comments quote some values (`okhttp/4.12.0 ...`); keep them true.
        client_text = client_text.replace(f"`{old}`", f"`{value}`")
        contract_text = contract_text.replace(json.dumps(old), json.dumps(value))

    if not changed:
        print("No client.rs changes needed.")
        return 0
    spec.client_rs.write_text(client_text, encoding="utf-8")
    CONTRACT_PY.write_text(contract_text, encoding="utf-8")
    print(f"Updated {spec.client_rs.relative_to(ROOT).as_posix()}:")
    for line in changed:
        print(f"  - {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
