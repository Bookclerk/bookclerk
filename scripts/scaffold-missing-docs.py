#!/usr/bin/env python3
"""Insert Google-style one-line rustdoc on undocumented public items.

Parses `cargo rustdoc -W missing_docs` output and inserts a concise `///`
summary above each reported span. Intended to clear workspace `missing_docs`
for fields/variants/modules; authors should expand functions/traits with
Arguments / Returns / Errors sections (see docs/code-documentation.md).

Usage:
  python3 scripts/scaffold-missing-docs.py bookclerk-library
  python3 scripts/scaffold-missing-docs.py --all
  python3 scripts/scaffold-missing-docs.py --all --dry-run
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

WARNING_RE = re.compile(
    r"warning: missing documentation for ([^\n]+)\n\s+--> ([^:]+):(\d+):",
    re.MULTILINE,
)

GLOSSARY = {
    "asin": "Amazon ASIN identifier",
    "isbn": "ISBN identifier",
    "api": "API",
    "id": "Identifier",
    "ids": "Identifiers",
    "url": "URL",
    "uri": "URI",
    "uuid": "UUID",
    "dto": "Wire DTO",
    "db": "Database",
    "rpc": "RPC",
    "s3": "S3",
    "abs": "Audiobookshelf",
    "oidc": "OIDC",
    "dek": "Data-encryption key",
    "hmac": "HMAC",
    "sha": "SHA",
    "json": "JSON",
    "toml": "TOML",
    "html": "HTML",
    "http": "HTTP",
    "https": "HTTPS",
    "cli": "CLI",
    "ui": "UI",
    "mp3": "MP3",
    "mp4": "MP4",
    "aax": "AAX",
    "aaxc": "AAXC",
    "cdm": "CDM",
    "drm": "DRM",
}


def lib_crate_names() -> list[str]:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            text=True,
            cwd=ROOT,
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


def missing_for(crate: str) -> list[tuple[str, Path, int]]:
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
        cwd=ROOT,
    )
    text = ANSI_RE.sub("", proc.stderr + proc.stdout)
    out: list[tuple[str, Path, int]] = []
    for kind, path, line in WARNING_RE.findall(text):
        p = Path(path)
        if not p.is_absolute():
            p = ROOT / p
        out.append((kind.strip(), p, int(line)))
    return out


def humanize_ident(ident: str) -> str:
    parts = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", ident).split("_")
    words: list[str] = []
    for part in parts:
        if not part:
            continue
        key = part.lower()
        words.append(GLOSSARY.get(key, key))
    text = " ".join(words).strip()
    if not text:
        return "Undocumented item"
    return text[0].upper() + text[1:]


def extract_ident(line: str) -> str | None:
    s = line.strip()
    # pub struct Foo / pub enum Foo / pub trait Foo / pub type Foo
    m = re.match(
        r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
        r"(?:struct|enum|trait|type|fn|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)",
        s,
    )
    if m:
        return m.group(1)
    # field: pub name: Type / name: Type /
    m = re.match(r"(?:pub(?:\([^)]*\))?\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*(?::|,|\()", s)
    if m and m.group(1) not in {"pub", "self", "crate", "super", "in", "mut", "ref"}:
        return m.group(1)
    # enum variant Foo / Foo( / Foo {
    m = re.match(r"([A-Za-z_][A-Za-z0-9_]*)\s*(?:\{|\(|,|$)", s)
    if m:
        return m.group(1)
    return None


def summary_for(kind: str, ident: str | None, line: str) -> str:
    name = ident or extract_ident(line) or "item"
    base = humanize_ident(name)
    if "struct field" in kind:
        return f"{base}."
    if "variant" in kind:
        return f"{base} variant."
    if "module" in kind:
        return f"{base} module."
    if "trait" in kind:
        return f"{base} trait."
    if "struct" in kind:
        return f"{base}."
    if "enum" in kind:
        return f"{base}."
    if "function" in kind or "method" in kind or "associated function" in kind:
        return f"{base}."
    if "constant" in kind or "static" in kind:
        return f"{base}."
    if "type alias" in kind:
        return f"{base} type alias."
    return f"{base}."


def already_documented(lines: list[str], idx: int) -> bool:
    # idx is 0-based line of the item.
    j = idx - 1
    while j >= 0 and lines[j].strip() == "":
        j -= 1
    if j < 0:
        return False
    s = lines[j].lstrip()
    return s.startswith("///") or s.startswith("//!") or s.startswith("#[doc")


def insert_doc(path: Path, line_no: int, kind: str, dry_run: bool) -> bool:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    idx = line_no - 1
    if idx < 0 or idx >= len(lines):
        return False
    if already_documented(lines, idx):
        return False
    # Skip if attributes above — place doc above attributes attached to the item.
    insert_at = idx
    while insert_at > 0:
        prev = lines[insert_at - 1].lstrip()
        if prev.startswith("#[") or prev.startswith("#!["):
            insert_at -= 1
            continue
        if prev.startswith("///") or prev.startswith("//!"):
            return False
        break
    item_line = lines[idx]
    indent = re.match(r"[ \t]*", item_line).group(0)
    # Module inner docs use //! only for crate/file roots; item modules use ///.
    doc = summary_for(kind, extract_ident(item_line), item_line)
    doc_line = f"{indent}/// {doc}\n"
    if dry_run:
        print(f"would document {path}:{line_no} ({kind}): {doc}")
        return True
    lines.insert(insert_at, doc_line)
    path.write_text("".join(lines), encoding="utf-8")
    return True


def process_crate(crate: str, dry_run: bool) -> int:
    items = missing_for(crate)
    # Apply bottom-up so line numbers stay valid within a file.
    items_sorted = sorted(items, key=lambda t: (str(t[1]), -t[2]))
    count = 0
    # Group by file and adjust offsets as we insert within the same file.
    by_file: dict[Path, list[tuple[str, int]]] = {}
    for kind, path, line in items_sorted:
        by_file.setdefault(path, []).append((kind, line))
    for path, entries in by_file.items():
        # entries already reverse-sorted by line within file from items_sorted
        entries = sorted(entries, key=lambda t: -t[1])
        for kind, line in entries:
            if not path.is_file():
                print(f"skip missing file {path}", file=sys.stderr)
                continue
            if insert_doc(path, line, kind, dry_run=dry_run):
                count += 1
    print(f"{crate}: scaffolded {count}/{len(items)}")
    return count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("crates", nargs="*", help="Workspace package names")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    crates = lib_crate_names() if args.all else args.crates
    if not crates:
        parser.error("pass crate names or --all")
    total = 0
    for crate in crates:
        total += process_crate(crate, dry_run=args.dry_run)
    print(f"TOTAL scaffolded {total}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
