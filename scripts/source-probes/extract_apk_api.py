#!/usr/bin/env python3
"""Extract the reverse-engineered API surface of a source's Android APK.

Covers the sources whose Rust clients follow an Android app but have no
dedicated probe of their own (Libro.fm has `scripts/librofm-apk-probe/`):

  graphicaudio — `com.efsharp.graphicaudio`, Retrofit REST API
  chirp        — `com.chirpbooks.chirp`, Apollo GraphQL ("Mockingjay")

Steps (download and decompile helpers are shared with the Libro.fm probe):

  1. Fetch the latest APK via apkeep (APKPure, no credentials)
  2. Decompile with jadx
  3. Extract the values behind each tracked client constant
  4. Diff against the source's `client.rs` and write `report.{json,md}` with a
     `suggested` map that `apply_apk_updates.py` can write back

Only values the APK pins down unambiguously become suggestions. When a tracked
value drifted but the APK offers no single replacement (an endpoint moved and
two candidates match, the base URL is gone, ...), the drift is reported with
`apk = null` and left for a person, so the fixer never guesses.

Exit codes:
  0 — extraction succeeded and tracked constants match
  1 — extraction succeeded but tracked constants drifted
  2 — hard failure (download / decompile / parse / bad args)
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]

# Reuse the Libro.fm probe's apkeep / jadx plumbing rather than copying it.
sys.path.insert(0, str(ROOT / "scripts" / "librofm-apk-probe"))
from extract_libro_api import (  # noqa: E402
    DEFAULT_APKEEP_VERSION,
    DEFAULT_JADX_VERSION,
    HTTP_METHOD_RE,
    RETROFIT_RE,
    decompile,
    ensure_apkeep,
    ensure_jadx,
    fetch_apk,
    log,
    parse_okhttp_ua,
    resolve_base_apk,
)

# `pub const NAME: &str = "value";` and private `const NAME: &str = "value";`.
CONST_RE = re.compile(
    r'^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+(?P<name>[A-Z0-9_]+)\s*:\s*&str\s*=\s*"(?P<value>[^"]*)"\s*;',
    re.MULTILINE,
)
# `query Name(` / `mutation Name(` inside the Rust raw-string documents.
GRAPHQL_OP_RE = re.compile(r"\b(query|mutation)\s+([A-Za-z_][A-Za-z0-9_]*)\s*[({]")
# Apollo Kotlin/Java generated operations.
APOLLO_OP_NAME_RE = re.compile(r'OPERATION_NAME\s*=\s*"([A-Za-z_][A-Za-z0-9_]*)"')
JAVA_URL_RE = re.compile(r'"(https://[^"\s]+)"')
JAVA_STRING_FIELD_RE = re.compile(
    r'public\s+static\s+final\s+String\s+(?P<name>\w+)\s*=\s*"(?P<value>[^"]*)"\s*;'
)
OKHTTP_TOKEN_RE = re.compile(r"^okhttp/[0-9][0-9A-Za-z.\-]*")


@dataclass
class Drift:
    """One tracked constant whose client value differs from the APK.

    `apk` is the replacement the APK pins down, or None when the APK shows the
    value drifted but offers no single replacement.
    """

    field: str
    client: str | None
    apk: str | None
    severity: str
    note: str = ""


@dataclass
class Extraction:
    """What a source extractor found in the decompiled APK."""

    # Constant name -> value implied by the APK (only unambiguous values).
    values: dict[str, str] = field(default_factory=dict)
    # Constant name -> why no value could be pinned down.
    unresolved: dict[str, str] = field(default_factory=dict)
    # Free-form facts shown in the report (endpoint list, operations, ...).
    facts: dict[str, Any] = field(default_factory=dict)
    # Informational drifts that never become suggestions.
    notes: list[Drift] = field(default_factory=list)


@dataclass(frozen=True)
class SourceSpec:
    """How to probe one source's APK and where its client constants live."""

    package: str
    client_rs: Path
    tracked: tuple[str, ...]
    extract: Callable[[Path, dict[str, str]], Extraction]


def read_java(sources: Path) -> list[tuple[Path, str]]:
    """Return (path, text) for every decompiled Java file, skipping libraries.

    # Arguments
    - `sources`: jadx `sources/` directory.

    # Returns
    App and generated sources; okhttp3 / kotlin / androidx / google trees are
    left out because their URLs and annotations are not the app's API.
    """
    skip = ("okhttp3", "okio", "kotlin", "kotlinx", "androidx", "android", "com/google")
    out: list[tuple[Path, str]] = []
    for path in sources.rglob("*.java"):
        rel = path.relative_to(sources).as_posix()
        if rel.startswith(skip):
            continue
        out.append((path, path.read_text(encoding="utf-8", errors="replace")))
    return out


def host_in(host: str, domain: str) -> bool:
    """True when `host` is `domain` or one of its subdomains (not `evil<domain>`)."""
    return host == domain or host.endswith("." + domain)


def parse_client_consts(client_rs: Path) -> dict[str, str]:
    """Return every `&str` constant declared in a Rust client file."""
    text = client_rs.read_text(encoding="utf-8")
    return {m.group("name"): m.group("value") for m in CONST_RE.finditer(text)}


def okhttp_value(sources: Path, current: str | None) -> tuple[str | None, str | None]:
    """Return the client UA with its `okhttp/x.y.z` token taken from the APK.

    The suffix after the token (for example ` Chirp/Bookclerk`) is ours and is
    kept. Returns `(value, None)` or `(None, reason)`.
    """
    apk_ua = parse_okhttp_ua(sources)
    if not apk_ua:
        return None, "OkHttp version not found in the APK"
    if current is None or not OKHTTP_TOKEN_RE.match(current):
        return None, "client USER_AGENT_VALUE does not start with okhttp/<version>"
    token = OKHTTP_TOKEN_RE.match(apk_ua)
    if token is None:
        return None, "OkHttp version not found in the APK"
    return OKHTTP_TOKEN_RE.sub(token.group(0), current, count=1), None


def retrofit_paths(files: list[tuple[Path, str]]) -> list[tuple[str, str]]:
    """Return sorted, de-duplicated (method, path) pairs from Retrofit annotations."""
    seen: set[tuple[str, str]] = set()
    for _, text in files:
        for method, path in RETROFIT_RE.findall(text):
            if method != "HTTP":
                seen.add((method, path))
        for method, path in HTTP_METHOD_RE.findall(text):
            seen.add((method.upper(), path))
    return sorted(seen, key=lambda mp: (mp[1], mp[0]))


def client_path(base_url: str, retrofit_path: str) -> str | None:
    """Resolve a Retrofit path against the base URL as a client `*_PATH` const.

    Retrofit resolves relative paths against the base URL (which ends in `/`)
    and absolute ones against the host. Client paths are relative to the base
    URL without its trailing slash and start with `/`. Returns None when the
    endpoint falls outside the base URL or is a full URL.
    """
    if "://" in retrofit_path:
        return None
    parts = urlsplit(base_url)
    base_path = parts.path.rstrip("/")
    if retrofit_path.startswith("/"):
        if not retrofit_path.startswith(base_path + "/"):
            return None
        rel = retrofit_path[len(base_path) :]
    else:
        rel = "/" + retrofit_path
    return rel.split("?", 1)[0]


def match_endpoint(current: str, candidates: list[str]) -> tuple[str | None, str | None]:
    """Pick the APK endpoint that replaces a client path, conservatively.

    The client value wins when the APK still has it. Otherwise exactly one APK
    path must end in the same final segment (`/activation/login` ->
    `.../login`); zero or several candidates leave the value unresolved.
    """
    if current in candidates:
        return current, None
    tail = current.rstrip("/").rsplit("/", 1)[-1]
    matches = sorted({c for c in candidates if c.rstrip("/").rsplit("/", 1)[-1] == tail})
    if len(matches) == 1:
        return matches[0], None
    if not matches:
        return None, f"no APK endpoint ends in /{tail}"
    return None, f"several APK endpoints end in /{tail}: {', '.join(matches)}"


def extract_graphicaudio(sources: Path, client: dict[str, str]) -> Extraction:
    """GraphicAudio: Retrofit base URL, the four tracked paths, OkHttp UA."""
    files = read_java(sources)
    ex = Extraction()
    endpoints = retrofit_paths(files)
    ex.facts["endpoints"] = [f"{m} {p}" for m, p in endpoints]

    # Base URL: graphicaudio.net URLs in files that configure Retrofit, plus
    # BuildConfig string fields. The `/access` API base is the one that hosts
    # the Retrofit interfaces; the storefront root is Magento (web, untracked).
    # The client's base still being an app literal anywhere counts as a match.
    current_base = client.get("DEFAULT_BASE_URL")
    all_urls: set[str] = set()
    candidates: set[str] = set()
    for path, text in files:
        configures = "baseUrl(" in text or path.name == "BuildConfig.java"
        for url in JAVA_URL_RE.findall(text):
            parts = urlsplit(url)
            if not host_in(parts.hostname or "", "graphicaudio.net"):
                continue
            all_urls.add(url)
            if configures and parts.path.strip("/"):
                candidates.add(url.rstrip("/"))
    ex.facts["graphicaudio_urls"] = sorted(all_urls)
    ex.facts["base_url_candidates"] = sorted(candidates)
    base: str | None = None
    if current_base and current_base.rstrip("/") in {u.rstrip("/") for u in all_urls}:
        base = current_base.rstrip("/")
    elif len(candidates) == 1:
        base = next(iter(candidates))
    elif not candidates:
        ex.unresolved["DEFAULT_BASE_URL"] = "no graphicaudio.net API base URL found"
    else:
        ex.unresolved["DEFAULT_BASE_URL"] = "several base URL candidates: " + ", ".join(
            sorted(candidates)
        )
    if base is not None:
        ex.values["DEFAULT_BASE_URL"] = base

    resolve_base = base or current_base
    rel_paths: list[str] = []
    if resolve_base:
        for _, p in endpoints:
            rel = client_path(resolve_base + "/", p)
            if rel is not None:
                rel_paths.append(rel)
    for name in ("LOGIN_PATH", "REMOVE_PATH", "PRODUCTS_PATH", "LINKS_PATH"):
        current = client.get(name)
        if current is None:
            ex.unresolved[name] = "constant missing from client.rs"
            continue
        value, why = match_endpoint(current, rel_paths)
        if value is not None:
            ex.values[name] = value
        else:
            ex.unresolved[name] = why or "unresolved"

    value, why = okhttp_value(sources, client.get("USER_AGENT_VALUE"))
    if value is not None:
        ex.values["USER_AGENT_VALUE"] = value
    else:
        ex.unresolved["USER_AGENT_VALUE"] = why or "unresolved"
    return ex


def extract_chirp(sources: Path, client: dict[str, str]) -> Extraction:
    """Chirp: Mockingjay GraphQL endpoint, OkHttp UA, Apollo operation names."""
    files = read_java(sources)
    ex = Extraction()

    # The app keeps its API root in BuildConfig (`MOCKINGJAY_API_ROOT`) and
    # appends `/graphql`; a full `.../graphql` literal is accepted as well.
    current = client.get("DEFAULT_GRAPHQL_URL")
    roots: set[str] = set()
    full: set[str] = set()
    for path, text in files:
        if path.name == "BuildConfig.java":
            for m in JAVA_STRING_FIELD_RE.finditer(text):
                if m.group("name") == "MOCKINGJAY_API_ROOT" and m.group("value").startswith("https://"):
                    roots.add(m.group("value").rstrip("/"))
        for url in JAVA_URL_RE.findall(text):
            host = urlsplit(url).hostname or ""
            if host_in(host, "chirpbooks.com") and url.rstrip("/").endswith("/graphql"):
                full.add(url.rstrip("/"))
    candidates = full | {r + "/graphql" for r in roots}
    ex.facts["graphql_url_candidates"] = sorted(candidates)
    if current and current in candidates:
        ex.values["DEFAULT_GRAPHQL_URL"] = current
    elif len(candidates) == 1:
        ex.values["DEFAULT_GRAPHQL_URL"] = next(iter(candidates))
    elif not candidates:
        ex.unresolved["DEFAULT_GRAPHQL_URL"] = "no MOCKINGJAY_API_ROOT or chirpbooks.com /graphql URL found"
    else:
        ex.unresolved["DEFAULT_GRAPHQL_URL"] = "several GraphQL URL candidates: " + ", ".join(
            sorted(candidates)
        )

    value, why = okhttp_value(sources, client.get("USER_AGENT_VALUE"))
    if value is not None:
        ex.values["USER_AGENT_VALUE"] = value
    else:
        ex.unresolved["USER_AGENT_VALUE"] = why or "unresolved"

    # Operation names are informational: the server does not key on them, and
    # the Rust documents are hand-written, so renames are never auto-applied.
    apk_ops: set[str] = set()
    for _, text in files:
        apk_ops.update(APOLLO_OP_NAME_RE.findall(text))
    rust_text = SPECS["chirp"].client_rs.read_text(encoding="utf-8")
    rust_ops = sorted({name for _, name in GRAPHQL_OP_RE.findall(rust_text)})
    ex.facts["apk_operations"] = sorted(apk_ops)
    ex.facts["client_operations"] = rust_ops
    if apk_ops:
        for op in rust_ops:
            if op not in apk_ops:
                ex.notes.append(
                    Drift(
                        field=f"operation.{op}",
                        client=op,
                        apk=None,
                        severity="info",
                        note="operation not in the APK (renamed, removed, or web-only)",
                    )
                )
    return ex


SPECS: dict[str, SourceSpec] = {
    "graphicaudio": SourceSpec(
        package="com.efsharp.graphicaudio",
        client_rs=ROOT / "crates/bookclerk-plugins/optional/source-graphicaudio/src/client.rs",
        tracked=(
            "DEFAULT_BASE_URL",
            "LOGIN_PATH",
            "REMOVE_PATH",
            "PRODUCTS_PATH",
            "LINKS_PATH",
            "USER_AGENT_VALUE",
        ),
        extract=extract_graphicaudio,
    ),
    "chirp": SourceSpec(
        package="com.chirpbooks.chirp",
        client_rs=ROOT / "crates/bookclerk-plugins/optional/source-chirp/src/client.rs",
        tracked=("DEFAULT_GRAPHQL_URL", "USER_AGENT_VALUE"),
        extract=extract_chirp,
    ),
}


def compare(spec: SourceSpec, client: dict[str, str], ex: Extraction) -> list[Drift]:
    """Diff tracked client constants against the extraction.

    An unresolved constant is blocking drift only when the APK no longer
    contains the client's current value anywhere it was looked for; that is
    what `unresolved` records, since a still-present value always resolves.
    """
    drifts: list[Drift] = []
    for name in spec.tracked:
        current = client.get(name)
        if name in ex.values:
            if ex.values[name] != current:
                drifts.append(Drift(name, current, ex.values[name], "error"))
        elif name in ex.unresolved:
            drifts.append(Drift(name, current, None, "error", ex.unresolved[name]))
    return drifts + ex.notes


def render_markdown(source: str, spec: SourceSpec, meta: dict[str, Any], client: dict[str, str], ex: Extraction, drifts: list[Drift]) -> str:
    """Render the human-readable report embedded in drift issues."""
    lines = [f"# {source} APK API probe", ""]
    lines.append(f"- Package: `{spec.package}`")
    version = meta.get("version_name")
    if version:
        lines.append(f"- APK version: `{version}`")
    lines.append(f"- Client: `{spec.client_rs.relative_to(ROOT).as_posix()}`")
    lines.append("")
    lines.append("## Drift")
    lines.append("")
    if not drifts:
        lines.append("No tracked constant drift.")
    else:
        lines.append("| Constant | Client | APK | Severity | Note |")
        lines.append("| --- | --- | --- | --- | --- |")
        for d in drifts:
            apk = f"`{d.apk}`" if d.apk is not None else "_no single replacement_"
            lines.append(f"| `{d.field}` | `{d.client}` | {apk} | {d.severity} | {d.note} |")
    lines.append("")
    lines.append("## Tracked constants")
    lines.append("")
    lines.append("| Constant | Client | APK |")
    lines.append("| --- | --- | --- |")
    for name in spec.tracked:
        apk = ex.values.get(name)
        lines.append(f"| `{name}` | `{client.get(name)}` | {f'`{apk}`' if apk else '—'} |")
    lines.append("")
    for key, value in sorted(ex.facts.items()):
        lines.append(f"<details><summary>{key}</summary>")
        lines.append("")
        lines.append("```")
        if isinstance(value, list):
            lines.extend(str(v) for v in value)
        else:
            lines.append(str(value))
        lines.append("```")
        lines.append("")
        lines.append("</details>")
        lines.append("")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; see the module docstring for exit codes."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", required=True, choices=sorted(SPECS))
    parser.add_argument("--out-dir", type=Path, default=None, help="Default: artifacts/<source>-apk-probe")
    parser.add_argument("--workdir", type=Path, default=None, help="Scratch directory (default: temporary)")
    parser.add_argument("--apk", type=Path, default=None, help="Use an existing .apk/.xapk instead of downloading")
    parser.add_argument("--apkeep-version", default=DEFAULT_APKEEP_VERSION)
    parser.add_argument("--jadx-version", default=DEFAULT_JADX_VERSION)
    parser.add_argument(
        "--fail-on-drift",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Exit 1 when tracked client.rs constants differ from the APK",
    )
    args = parser.parse_args(argv)

    spec = SPECS[args.source]
    if not spec.client_rs.exists():
        log(f"error: missing {spec.client_rs}")
        return 2
    out_dir = (args.out_dir or ROOT / "artifacts" / f"{args.source}-apk-probe").resolve()
    keep_work = args.workdir is not None
    work = (args.workdir or Path(tempfile.mkdtemp(prefix=f"{args.source}-apk-"))).resolve()
    tools = work / "tools"
    tools.mkdir(parents=True, exist_ok=True)

    try:
        if args.apk:
            package_path = args.apk.resolve()
            if not package_path.exists():
                log(f"error: --apk not found: {package_path}")
                return 2
        else:
            apkeep = ensure_apkeep(tools, args.apkeep_version)
            package_path = fetch_apk(apkeep, work / "download", spec.package)
        base_apk, meta = resolve_base_apk(package_path, work)
        log(f"Base APK: {base_apk} ({base_apk.stat().st_size} bytes)")
        jadx = ensure_jadx(tools, args.jadx_version)
        sources = decompile(jadx, base_apk, work / "jadx-out")

        client = parse_client_consts(spec.client_rs)
        ex = spec.extract(sources, client)
        drifts = compare(spec, client, ex)
        blocking = [d for d in drifts if d.severity == "error"]
        suggested = {d.field: d.apk for d in blocking if d.apk is not None}

        out_dir.mkdir(parents=True, exist_ok=True)
        payload = {
            "source": args.source,
            "package": spec.package,
            "apk_version": meta.get("version_name"),
            "client": {name: client.get(name) for name in spec.tracked},
            "apk": ex.values,
            "unresolved": ex.unresolved,
            "facts": ex.facts,
            "drifts": [d.__dict__ for d in drifts],
            "suggested": suggested,
            "has_blocking_drift": bool(blocking),
            # True only when every blocking drift has a replacement.
            "fully_fixable": bool(blocking) and len(suggested) == len(blocking),
        }
        (out_dir / "report.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        (out_dir / "report.md").write_text(
            render_markdown(args.source, spec, meta, client, ex, drifts) + "\n", encoding="utf-8"
        )
        log(f"Wrote {out_dir / 'report.md'}")
        for key, value in sorted(ex.facts.items()):
            shown = value[:40] if isinstance(value, list) else value
            log(f"{key}: {json.dumps(shown)}")
        if blocking:
            log(f"Detected {len(blocking)} tracked drift(s):")
            for d in blocking:
                log(f"  - {d.field}: apk={d.apk!r} client={d.client!r} {d.note}")
            return 1 if args.fail_on_drift else 0
        log("No tracked constant drift.")
        return 0
    except Exception as exc:  # noqa: BLE001 — top-level CLI boundary
        log(f"error: {exc}")
        return 2
    finally:
        if not keep_work:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
