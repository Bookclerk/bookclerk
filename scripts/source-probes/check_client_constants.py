#!/usr/bin/env python3
"""Assert reverse-engineered client constants still match probe expectations.

Reads path/URL string literals from the Rust clients and fails if the probe
contract drifts (catch rename/typo before CI live smoke).
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

CONTRACTS = {
    "graphicaudio": {
        "client": ROOT / "crates/bookclerk-graphicaudio/src/client.rs",
        "must_contain": [
            "https://www.graphicaudio.net/access",
            "/activation/login",
            "/activation/remove",
            "/api/products",
            "/api/links",
        ],
        "magento": ROOT / "crates/bookclerk-graphicaudio/src/magento.rs",
        "magento_must_contain": [
            "https://www.graphicaudio.net",
            "/downloadable/customer/products/",
            "/library/index/content_library",
        ],
    },
    "chirp": {
        "client": ROOT / "crates/bookclerk-chirp/src/client.rs",
        "must_contain": [
            "https://api.chirpbooks.com/api/graphql",
            "mutation signIn",
            "AndroidCurrentUserAudiobooks",
            "AndroidSingleAudiobook",
            "CHIRP_AUDIO",
        ],
    },
}


def check_source(name: str) -> list[str]:
    cfg = CONTRACTS[name]
    errors: list[str] = []
    text = cfg["client"].read_text(encoding="utf-8")
    for needle in cfg["must_contain"]:
        if needle not in text:
            errors.append(f"{name}: missing in {cfg['client'].name}: {needle!r}")
    if "magento" in cfg:
        mtext = cfg["magento"].read_text(encoding="utf-8")
        for needle in cfg["magento_must_contain"]:
            if needle not in mtext:
                errors.append(f"{name}: missing in magento.rs: {needle!r}")
    # Pub const sanity: LOGIN_PATH style assignments should be string literals.
    for match in re.finditer(
        r'pub const ([A-Z0-9_]+):\s*&str\s*=\s*"([^"]*)"', text
    ):
        ident, value = match.group(1), match.group(2)
        if not value:
            errors.append(f"{name}: empty const {ident}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--sources",
        default="graphicaudio,chirp",
        help="Comma-separated sources to check",
    )
    args = parser.parse_args()
    sources = [s.strip() for s in args.sources.split(",") if s.strip()]
    errors: list[str] = []
    for source in sources:
        if source not in CONTRACTS:
            errors.append(f"unknown source {source!r}")
            continue
        errors.extend(check_source(source))
    if errors:
        print("client constant contract failures:")
        for err in errors:
            print(f"  - {err}")
        return 1
    print(f"ok — checked {', '.join(sources)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
