#!/usr/bin/env python3
"""Bump the pinned Cloudflare workerd release (SoT: workerd-pin.json).

Selects the newest GitHub release whose published_at is at least COOLDOWN_DAYS
old (supply-chain cooldown, akin to Dependabot). Writes
`crates/bookclerk-workerd/workerd-pin.json`, then regenerates `pin.rs` and SDK
stub copies via `scripts/sync-workerd-pin.py`.

Exit codes:
  0 — pin updated (or --check with a candidate available)
  1 — error
  2 — already current / no eligible release older than the cooldown
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

REPO = "cloudflare/workerd"
ROOT = Path(__file__).resolve().parents[1]
PIN_JSON = ROOT / "crates/bookclerk-workerd/workerd-pin.json"
ASSETS = (
    "workerd-linux-64.gz",
    "workerd-linux-arm64.gz",
    "workerd-darwin-64.gz",
    "workerd-darwin-arm64.gz",
    "workerd-windows-64.gz",
)
ARTIFACT_TO_PLATFORM = {
    "workerd-linux-64.gz": "linux-x86_64",
    "workerd-linux-arm64.gz": "linux-aarch64",
    "workerd-darwin-64.gz": "macos-x86_64",
    "workerd-darwin-arm64.gz": "macos-aarch64",
    "workerd-windows-64.gz": "windows-x86_64",
}
DEFAULT_COOLDOWN_DAYS = 7


def _load_sync():
    path = ROOT / "scripts/sync-workerd-pin.py"
    spec = importlib.util.spec_from_file_location("sync_workerd_pin", path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"could not load {path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def gh_api(url: str) -> object:
    req = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "User-Agent": "bookclerk-bump-workerd-pin",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.load(resp)


def parse_tag_date(tag: str) -> str | None:
    """v1.YYYYMMDD.N → YYYY-MM-DD for bundled_compat_date."""
    m = re.fullmatch(r"v1\.(\d{4})(\d{2})(\d{2})(?:\.\d+)?", tag)
    if not m:
        return None
    return f"{m.group(1)}-{m.group(2)}-{m.group(3)}"


def current_pin() -> str:
    sync = _load_sync()
    data = sync.load_pin(PIN_JSON)
    return data["release_tag"]


def tag_sort_key(tag: str) -> tuple[int, int]:
    """Order Cloudflare tags like v1.YYYYMMDD.N."""
    m = re.fullmatch(r"v1\.(\d{8})\.(\d+)", tag)
    if not m:
        return (0, 0)
    return (int(m.group(1)), int(m.group(2)))


def digest_hex(asset: dict) -> str:
    digest = asset.get("digest") or ""
    if digest.startswith("sha256:"):
        return digest.removeprefix("sha256:")
    raise SystemExit(
        f"asset {asset.get('name')} missing sha256 digest (got {digest!r})"
    )


def pick_release(cooldown_days: int) -> dict | None:
    cutoff = datetime.now(timezone.utc) - timedelta(days=cooldown_days)
    # Paginate a bit; workerd releases daily so page 1–3 covers weeks.
    for page in range(1, 6):
        url = (
            f"https://api.github.com/repos/{REPO}/releases"
            f"?per_page=30&page={page}"
        )
        releases = gh_api(url)
        if not isinstance(releases, list) or not releases:
            break
        for rel in releases:
            if rel.get("draft") or rel.get("prerelease"):
                continue
            tag = rel.get("tag_name") or ""
            if not parse_tag_date(tag):
                continue
            published = rel.get("published_at") or rel.get("created_at")
            if not published:
                continue
            published_dt = datetime.fromisoformat(published.replace("Z", "+00:00"))
            if published_dt > cutoff:
                continue
            names = {a.get("name") for a in rel.get("assets") or []}
            if not all(name in names for name in ASSETS):
                continue
            return rel
    return None


def build_pin_data(tag: str, digests: dict[str, str], version_stamp: str) -> dict:
    compat = parse_tag_date(tag)
    if not compat:
        raise SystemExit(f"unrecognized workerd tag shape: {tag}")
    assets = {}
    for artifact, sha in digests.items():
        platform = ARTIFACT_TO_PLATFORM[artifact]
        assets[platform] = {"artifact": artifact, "sha256_hex": sha}
    return {
        "release_tag": tag,
        "bundled_compat_date": compat,
        "version_stamp": version_stamp,
        "assets": assets,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cooldown-days",
        type=int,
        default=DEFAULT_COOLDOWN_DAYS,
        help=f"only consider releases at least this many days old (default {DEFAULT_COOLDOWN_DAYS})",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit 0 if a bump is available, 2 if pin is current; do not write",
    )
    parser.add_argument(
        "--write",
        action="store_true",
        help="write workerd-pin.json (+ sync pin.rs / stubs) when a newer eligible release exists",
    )
    args = parser.parse_args()
    if not args.check and not args.write:
        args.write = True

    sync = _load_sync()
    current = current_pin()
    version_stamp = sync.load_pin(PIN_JSON).get("version_stamp", "workerd.version")

    try:
        rel = pick_release(args.cooldown_days)
    except urllib.error.URLError as err:
        print(f"error: GitHub API: {err}", file=sys.stderr)
        return 1

    if rel is None:
        print(
            f"no workerd release older than {args.cooldown_days}d with required assets",
            file=sys.stderr,
        )
        return 2

    tag = rel["tag_name"]
    if tag == current:
        print(f"pin already current: {current} (cooldown={args.cooldown_days}d)")
        return 2
    if tag_sort_key(tag) <= tag_sort_key(current):
        # Pin is newer than the newest release that cleared the cooldown
        # (manual/latest pin, or waiting for the next eligible publish).
        print(
            f"no newer eligible workerd release than {current} "
            f"(newest ≥{args.cooldown_days}d old is {tag})",
            file=sys.stderr,
        )
        return 2

    digests = {}
    for asset in rel.get("assets") or []:
        name = asset.get("name")
        if name in ASSETS:
            digests[name] = digest_hex(asset)
    missing = [n for n in ASSETS if n not in digests]
    if missing:
        print(f"error: missing digests for {missing}", file=sys.stderr)
        return 1

    print(f"bump workerd pin: {current} → {tag} (cooldown={args.cooldown_days}d)")
    if args.check:
        return 0

    data = build_pin_data(tag, digests, version_stamp)
    body = sync.canonical_json(data)
    PIN_JSON.write_text(body, encoding="utf-8")
    print(f"wrote {PIN_JSON.relative_to(ROOT)}")

    written = sync.write_derived(data)
    for path in written:
        if path.resolve() != PIN_JSON.resolve():
            print(f"wrote {path.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
