#!/usr/bin/env python3
"""Download the Libro.fm Android APK and extract unofficial API surface.

The official mobile client is the source of truth for community reverse-
engineered clients (including crates/bookclerk-libro). This script:

  1. Fetches the latest APK via apkeep (APKPure by default — no credentials)
  2. Decompiles with jadx
  3. Extracts Retrofit endpoints, auth headers, base URL / API version,
     OkHttp user-agent, and app version metadata
  4. Diffs against the constants in crates/bookclerk-libro/src/client.rs

Exit codes:
  0 — extraction succeeded and tracked constants match
  1 — extraction succeeded but tracked constants drifted
  2 — hard failure (download / decompile / parse)
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import zipfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

# Allow `python3 scripts/librofm-apk-probe/extract_libro_api.py` without install.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from schema_extract import (  # noqa: E402
    build_tracked_schema,
    compare_shapes,
    contracts_as_dict,
    parse_api_contracts,
)

PACKAGE_NAME = "fm.libro.librofm"
DEFAULT_APKEEP_VERSION = "0.17.0"
DEFAULT_JADX_VERSION = "1.5.1"

# Paths / headers we actively use in bookclerk-libro and want to keep in sync.
TRACKED_RELATIVE_PATHS = {
    "library",
    "download-manifest",
    "audiobooks/{isbn}/packaged_m4b",
}
TRACKED_OAUTH_SUFFIX = "/oauth/token"

CONST_RE = re.compile(
    r'pub\s+const\s+(?P<name>[A-Z0-9_]+)\s*:\s*&str\s*=\s*"(?P<value>[^"]*)"\s*;'
)
RETROFIT_RE = re.compile(
    r'@(GET|POST|PUT|DELETE|PATCH|HTTP)\((?:value\s*=\s*)?"([^"]*)"\)'
)
HTTP_METHOD_RE = re.compile(r'@HTTP\([^)]*method\s*=\s*"([^"]+)"[^)]*path\s*=\s*"([^"]*)"')
HEADER_ADD_RE = re.compile(r'addHeader\(\s*"([^"]+)"')
BUILDCONFIG_FIELD_RE = re.compile(
    r'public\s+static\s+final\s+(?:String|int|boolean|Boolean)\s+(\w+)\s*=\s*(.+?);'
)
OKHTTP_UA_RE = re.compile(r'userAgent\s*=\s*"([^"]+)"')
OKHTTP_VER_RE = re.compile(r'public\s+static\s+final\s+String\s+VERSION\s*=\s*"([^"]+)"')
OAUTH_TOKEN_RE = re.compile(r'"/oauth/token"|\'/oauth/token\'')
API_PREFIX_RE = re.compile(r'return\s+"(/api/v\d+/)"\s*;')
BASE_URL_RE = re.compile(r'return\s+"(https://[^"]+)"\s*;')


@dataclass
class Endpoint:
    method: str
    path: str
    source: str
    function: str | None = None
    query: list[str] = field(default_factory=list)
    path_params: list[str] = field(default_factory=list)
    request_body_type: str | None = None
    response_type: str | None = None


@dataclass
class ApkSurface:
    package_name: str = PACKAGE_NAME
    version_name: str | None = None
    version_code: str | int | None = None
    base_url: str | None = None
    api_prefix: str | None = None
    api_version: str | None = None
    okhttp_user_agent: str | None = None
    headers: list[str] = field(default_factory=list)
    oauth_token_path: str | None = None
    endpoints: list[Endpoint] = field(default_factory=list)
    build_config: dict[str, Any] = field(default_factory=dict)
    secrets_redacted: dict[str, str] = field(default_factory=dict)
    tracked_shapes: dict[str, Any] = field(default_factory=dict)
    endpoint_contracts: list[dict[str, Any]] = field(default_factory=list)


@dataclass
class ClientConsts:
    default_base_url: str | None = None
    oauth_token_path: str | None = None
    library_path: str | None = None
    download_manifest_path: str | None = None
    packaged_m4b_path: str | None = None
    app_ver: str | None = None
    user_agent_value: str | None = None
    client_id: str | None = None


def log(msg: str) -> None:
    print(msg, flush=True)


def run(cmd: list[str], *, cwd: Path | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    log(f"+ {' '.join(cmd)}")
    return subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        check=check,
        text=True,
        capture_output=True,
    )


def download(url: str, dest: Path) -> None:
    log(f"Downloading {url}")
    dest.parent.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(url) as resp, dest.open("wb") as out:
        shutil.copyfileobj(resp, out)


def ensure_apkeep(tools: Path, version: str) -> Path:
    binary = tools / "apkeep"
    if binary.exists():
        return binary
    url = (
        "https://github.com/EFForg/apkeep/releases/download/"
        f"{version}/apkeep-x86_64-unknown-linux-gnu"
    )
    download(url, binary)
    binary.chmod(0o755)
    return binary


def find_jadx_bin(root: Path) -> Path | None:
    """Locate ``bin/jadx`` under an extracted jadx tree.

    Official ``jadx-{ver}.zip`` archives place ``bin/`` at the archive root.
    Older or alternate builds may nest under ``jadx-{ver}/`` — accept either.
    """
    direct = root / "bin" / "jadx"
    if direct.is_file():
        return direct
    matches = [
        p
        for p in root.rglob("jadx")
        if p.is_file() and p.parent.name == "bin" and p.name == "jadx"
    ]
    if not matches:
        return None
    return min(matches, key=lambda p: len(p.parts))


def ensure_jadx(tools: Path, version: str) -> Path:
    extract_dir = tools / "jadx"
    existing = find_jadx_bin(extract_dir) if extract_dir.exists() else None
    if existing is not None:
        return existing
    archive = tools / f"jadx-{version}.zip"
    url = f"https://github.com/skylot/jadx/releases/download/v{version}/jadx-{version}.zip"
    download(url, archive)
    if extract_dir.exists():
        shutil.rmtree(extract_dir)
    extract_dir.mkdir(parents=True)
    with zipfile.ZipFile(archive) as zf:
        zf.extractall(extract_dir)
    jadx_bin = find_jadx_bin(extract_dir)
    if jadx_bin is None:
        raise RuntimeError(
            f"jadx binary not found under {extract_dir} after extracting {archive.name}"
        )
    jadx_bin.chmod(0o755)
    return jadx_bin


def fetch_apk(apkeep: Path, out_dir: Path, package: str) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    # Prefer APKPure: no Google credentials required in CI.
    proc = run([str(apkeep), "-a", package, "-d", "apk-pure", str(out_dir)], check=False)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout + proc.stderr)
        raise RuntimeError(f"apkeep failed with exit {proc.returncode}")
    candidates = sorted(
        list(out_dir.glob("*.xapk"))
        + list(out_dir.glob("*.apk"))
        + list(out_dir.glob("*.apks")),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    if not candidates:
        raise RuntimeError(f"apkeep produced no APK under {out_dir}")
    return candidates[0]


def resolve_base_apk(package_path: Path, work: Path) -> tuple[Path, dict[str, Any]]:
    """Return (base_apk, metadata). Handles plain APK and APKPure XAPK."""
    meta: dict[str, Any] = {"source_archive": str(package_path.name)}
    if package_path.suffix.lower() == ".apk":
        return package_path, meta

    unpack = work / "xapk"
    unpack.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(package_path) as zf:
        zf.extractall(unpack)

    manifest_path = unpack / "manifest.json"
    if manifest_path.exists():
        meta.update(json.loads(manifest_path.read_text(encoding="utf-8")))
        for split in meta.get("split_apks") or []:
            if split.get("id") == "base":
                return unpack / split["file"], meta

    # Fallback: largest .apk inside the archive.
    apks = sorted(unpack.glob("*.apk"), key=lambda p: p.stat().st_size, reverse=True)
    if not apks:
        raise RuntimeError(f"no APK found inside {package_path}")
    return apks[0], meta


def decompile(jadx: Path, apk: Path, out_dir: Path) -> Path:
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    proc = run(
        [
            str(jadx),
            "-d",
            str(out_dir),
            "--no-res",
            "--threads-count",
            str(os.cpu_count() or 2),
            "-q",
            str(apk),
        ],
        check=False,
    )
    # jadx may exit non-zero on partial decompilation; accept if sources exist.
    sources = out_dir / "sources"
    if not sources.exists():
        sys.stderr.write(proc.stdout + proc.stderr)
        raise RuntimeError("jadx produced no sources/")
    return sources


def _literal(value: str) -> Any:
    value = value.strip()
    if value.startswith('"') and value.endswith('"'):
        return value[1:-1]
    if value in {"true", "True"}:
        return True
    if value in {"false", "False"}:
        return False
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    return value


def parse_build_config(sources: Path) -> tuple[dict[str, Any], dict[str, str]]:
    path = sources / "fm" / "libro" / "librofm" / "BuildConfig.java"
    if not path.exists():
        return {}, {}
    text = path.read_text(encoding="utf-8", errors="replace")
    raw: dict[str, Any] = {}
    for match in BUILDCONFIG_FIELD_RE.finditer(text):
        raw[match.group(1)] = _literal(match.group(2))

    redacted: dict[str, str] = {}
    public: dict[str, Any] = {}
    for key, value in raw.items():
        if key.upper().endswith("KEY") or key.upper().endswith("SECRET") or "PASSWORD" in key.upper():
            digest = hashlib.sha256(str(value).encode("utf-8")).hexdigest()[:16]
            redacted[key] = f"sha256:{digest}"
            public[key] = "<redacted>"
        else:
            public[key] = value
    return public, redacted


def parse_api_prefix_and_base(sources: Path) -> tuple[str | None, str | None]:
    app_module = sources / "fm" / "libro" / "application" / "di" / "AppModule.java"
    prefix = base = None
    if app_module.exists():
        text = app_module.read_text(encoding="utf-8", errors="replace")
        m = API_PREFIX_RE.search(text)
        if m:
            prefix = m.group(1)
        m = BASE_URL_RE.search(text)
        if m:
            base = m.group(1)
    return base, prefix


def parse_headers(sources: Path) -> list[str]:
    path = sources / "fm" / "libro" / "rest" / "interceptors" / "AuthInterceptor.java"
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8", errors="replace")
    headers = HEADER_ADD_RE.findall(text)
    # Authorization is added via HttpHeaders.AUTHORIZATION constant.
    if "AUTHORIZATION" in text or "Authorization" in text:
        headers.append("Authorization")
    # Preserve order, unique.
    seen: set[str] = set()
    ordered: list[str] = []
    for h in headers:
        if h not in seen:
            seen.add(h)
            ordered.append(h)
    return ordered


def parse_okhttp_ua(sources: Path) -> str | None:
    util = sources / "okhttp3" / "internal" / "Util.java"
    if util.exists():
        m = OKHTTP_UA_RE.search(util.read_text(encoding="utf-8", errors="replace"))
        if m:
            return m.group(1)
    okhttp = sources / "okhttp3" / "OkHttp.java"
    if okhttp.exists():
        m = OKHTTP_VER_RE.search(okhttp.read_text(encoding="utf-8", errors="replace"))
        if m:
            return f"okhttp/{m.group(1)}"
    return None


def parse_oauth_path(sources: Path) -> str | None:
    # LoginRepoImpl builds baseUrl + "/oauth/token".
    for path in sources.rglob("LoginRepoImpl*.java"):
        text = path.read_text(encoding="utf-8", errors="replace")
        if OAUTH_TOKEN_RE.search(text) or "/oauth/token" in text:
            return TRACKED_OAUTH_SUFFIX
    return TRACKED_OAUTH_SUFFIX if any(sources.rglob("LoginApi.java")) else None


def parse_endpoints(sources: Path) -> list[Endpoint]:
    """Legacy thin endpoint list (method/path) plus richer contracts via schema_extract."""
    contracts = parse_api_contracts(sources)
    endpoints: list[Endpoint] = []
    seen: set[tuple[str, str]] = set()
    for c in contracts:
        key = (c.method, c.path)
        if key in seen:
            continue
        seen.add(key)
        endpoints.append(
            Endpoint(
                method=c.method,
                path=c.path,
                source=c.source,
                function=c.function,
                query=sorted(p.name for p in c.params if p.kind == "Query" and p.name),
                path_params=sorted(p.name for p in c.params if p.kind == "Path" and p.name),
                request_body_type=c.request_body_type,
                response_type=c.response_type,
            )
        )
    endpoints.sort(key=lambda e: (e.path, e.method, e.source))
    return endpoints


def extract_surface(sources: Path, meta: dict[str, Any]) -> ApkSurface:
    build_config, secrets = parse_build_config(sources)
    base_url, api_prefix = parse_api_prefix_and_base(sources)
    if not base_url:
        base_url = build_config.get("BASE_URL")
    if not api_prefix and build_config.get("API_VERSION"):
        api_prefix = f"/api/{build_config['API_VERSION']}/"

    contracts = parse_api_contracts(sources)
    return ApkSurface(
        version_name=meta.get("version_name") or build_config.get("VERSION_NAME"),
        version_code=meta.get("version_code") or build_config.get("VERSION_CODE"),
        base_url=base_url,
        api_prefix=api_prefix,
        api_version=build_config.get("API_VERSION"),
        okhttp_user_agent=parse_okhttp_ua(sources),
        headers=parse_headers(sources),
        oauth_token_path=parse_oauth_path(sources),
        endpoints=parse_endpoints(sources),
        build_config=build_config,
        secrets_redacted=secrets,
        tracked_shapes=build_tracked_schema(sources, contracts),
        endpoint_contracts=contracts_as_dict(contracts),
    )


def parse_client_rs(path: Path) -> ClientConsts:
    text = path.read_text(encoding="utf-8")
    consts = {m.group("name"): m.group("value") for m in CONST_RE.finditer(text)}
    return ClientConsts(
        default_base_url=consts.get("DEFAULT_BASE_URL"),
        oauth_token_path=consts.get("OAUTH_TOKEN_PATH"),
        library_path=consts.get("LIBRARY_PATH"),
        download_manifest_path=consts.get("DOWNLOAD_MANIFEST_PATH"),
        packaged_m4b_path=consts.get("PACKAGED_M4B_PATH"),
        app_ver=consts.get("APP_VER"),
        user_agent_value=consts.get("USER_AGENT_VALUE"),
        client_id=consts.get("CLIENT_ID"),
    )


def absolute_api_paths(surface: ApkSurface) -> dict[str, str]:
    """Map relative Retrofit paths we track to absolute /api/vN/... paths."""
    prefix = (surface.api_prefix or "").rstrip("/")
    out: dict[str, str] = {}
    relative = {e.path for e in surface.endpoints}
    for rel in TRACKED_RELATIVE_PATHS:
        if rel in relative and prefix:
            out[rel] = f"{prefix}/{rel}"
    return out


def compare(surface: ApkSurface, client: ClientConsts) -> list[dict[str, Any]]:
    drifts: list[dict[str, Any]] = []

    def note(field: str, expected_from_apk: Any, actual_in_client: Any, severity: str = "error") -> None:
        if expected_from_apk != actual_in_client:
            drifts.append(
                {
                    "field": field,
                    "apk": expected_from_apk,
                    "client": actual_in_client,
                    "severity": severity,
                }
            )

    note("DEFAULT_BASE_URL", surface.base_url, client.default_base_url)
    note("OAUTH_TOKEN_PATH", surface.oauth_token_path, client.oauth_token_path)
    note("APP_VER", surface.version_name, client.app_ver)
    note("USER_AGENT_VALUE", surface.okhttp_user_agent, client.user_agent_value)

    abs_paths = absolute_api_paths(surface)
    note("LIBRARY_PATH", abs_paths.get("library"), client.library_path)
    note(
        "DOWNLOAD_MANIFEST_PATH",
        abs_paths.get("download-manifest"),
        client.download_manifest_path,
    )
    note(
        "PACKAGED_M4B_PATH",
        abs_paths.get("audiobooks/{isbn}/packaged_m4b"),
        client.packaged_m4b_path,
    )

    # Informative: headers we do not yet send (except ones we already map).
    known = {
        "Authorization",
        "X-LibroFm-AppVer",
        "X-LibroFm-Device",
        "X-LibroFm-OsVer",
    }
    missing_headers = [h for h in surface.headers if h not in known]
    if missing_headers:
        drifts.append(
            {
                "field": "headers_not_in_client",
                "apk": missing_headers,
                "client": [
                    "Authorization",
                    "X-LibroFm-AppVer",
                    "X-LibroFm-Device",
                    "X-LibroFm-OsVer",
                    "User-Agent",
                    "Content-Type",
                    "Accept",
                ],
                "severity": "info",
            }
        )
    return drifts


def load_expected_shapes(repo: Path) -> dict[str, Any]:
    path = repo / "scripts" / "librofm-apk-probe" / "expected_shapes.json"
    if not path.exists():
        return {}
    data = json.loads(path.read_text(encoding="utf-8"))
    return {k: v for k, v in data.items() if not k.startswith("_")}


def compare_with_schema(
    surface: ApkSurface,
    client: ClientConsts,
    expected_shapes: dict[str, Any],
) -> list[dict[str, Any]]:
    drifts = compare(surface, client)
    drifts.extend(compare_shapes(surface.tracked_shapes, expected_shapes))
    return drifts


def render_markdown(
    surface: ApkSurface,
    client: ClientConsts,
    drifts: list[dict[str, Any]],
    meta: dict[str, Any],
) -> str:
    lines: list[str] = []
    lines.append("# Libro.fm APK API probe")
    lines.append("")
    lines.append(f"- Package: `{surface.package_name}`")
    lines.append(f"- App version: `{surface.version_name}` (code `{surface.version_code}`)")
    lines.append(f"- Base URL: `{surface.base_url}`")
    lines.append(f"- API prefix: `{surface.api_prefix}`")
    lines.append(f"- OkHttp UA: `{surface.okhttp_user_agent}`")
    lines.append(f"- Source archive: `{meta.get('source_archive')}`")
    lines.append("")
    lines.append("## Drift vs `crates/bookclerk-libro/src/client.rs` + `expected_shapes.json`")
    lines.append("")
    errors = [d for d in drifts if d["severity"] == "error"]
    infos = [d for d in drifts if d["severity"] != "error"]
    if not errors:
        lines.append("No tracked constant/schema drift detected.")
    else:
        lines.append("| Field | APK | Client / expected |")
        lines.append("| --- | --- | --- |")
        for d in errors:
            lines.append(f"| `{d['field']}` | `{d['apk']}` | `{d['client']}` |")
    if infos:
        lines.append("")
        lines.append("### Informational")
        lines.append("")
        for d in infos:
            lines.append(f"- **{d['field']}**: apk=`{d['apk']}` client=`{d['client']}`")
    lines.append("")
    lines.append("## Auth headers (AuthInterceptor)")
    lines.append("")
    for h in surface.headers:
        lines.append(f"- `{h}`")
    lines.append("")
    lines.append("## Tracked request/response shapes (from APK Gson/Retrofit)")
    lines.append("")
    lines.append("```json")
    lines.append(json.dumps(surface.tracked_shapes, indent=2, sort_keys=True))
    lines.append("```")
    lines.append("")
    lines.append("## Retrofit endpoints (relative to API prefix)")
    lines.append("")
    lines.append("| Method | Path | Query | Response | Source |")
    lines.append("| --- | --- | --- | --- | --- |")
    for e in surface.endpoints:
        q = ",".join(e.query) if e.query else ""
        lines.append(
            f"| `{e.method}` | `{e.path}` | `{q}` | `{e.response_type or ''}` | `{e.source}` |"
        )
    lines.append("")
    lines.append("## BuildConfig (secrets redacted)")
    lines.append("")
    lines.append("```json")
    lines.append(json.dumps(surface.build_config, indent=2, sort_keys=True))
    lines.append("```")
    if surface.secrets_redacted:
        lines.append("")
        lines.append("Secret field fingerprints:")
        for k, v in sorted(surface.secrets_redacted.items()):
            lines.append(f"- `{k}` → `{v}`")
    lines.append("")
    lines.append("## Current client constants")
    lines.append("")
    lines.append("```json")
    lines.append(json.dumps(asdict(client), indent=2, sort_keys=True))
    lines.append("```")
    lines.append("")
    return "\n".join(lines)


def drifts_need_live_smoke(drifts: list[dict[str, Any]]) -> bool:
    """True when APK surface changes warrant hitting the live Libro.fm API.

    Blocking constant drifts (paths/version/UA) and any schema.* drift (request
    or response field contracts) trigger live validation. Purely informational
    extras (e.g. unused X-LibroFm-Api-Key header) do not.
    """
    for d in drifts:
        field = str(d.get("field") or "")
        severity = d.get("severity")
        if severity == "error":
            return True
        if field.startswith("schema."):
            return True
    return False


def write_outputs(
    out_dir: Path,
    surface: ApkSurface,
    client: ClientConsts,
    drifts: list[dict[str, Any]],
    meta: dict[str, Any],
) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    blocking = any(d["severity"] == "error" for d in drifts)
    payload = {
        "apk": {
            **{k: v for k, v in asdict(surface).items() if k != "endpoints"},
            "endpoints": [asdict(e) for e in surface.endpoints],
            "absolute_tracked_paths": absolute_api_paths(surface),
        },
        "client": asdict(client),
        "drifts": drifts,
        "meta": meta,
        "has_blocking_drift": blocking,
        # Live oauth/library/download should only run when the APK API changed.
        "needs_live_smoke": drifts_need_live_smoke(drifts),
    }
    (out_dir / "report.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    (out_dir / "report.md").write_text(
        render_markdown(surface, client, drifts, meta), encoding="utf-8"
    )
    (out_dir / "apk_shapes.json").write_text(
        json.dumps(surface.tracked_shapes, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    endpoint_lines = [
        f"{e.method} {e.path} query=[{','.join(e.query)}] resp={e.response_type or '-'}"
        for e in surface.endpoints
    ]
    (out_dir / "endpoints.txt").write_text("\n".join(endpoint_lines) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="Repository root (default: inferred from script location)",
    )
    parser.add_argument(
        "--workdir",
        type=Path,
        default=None,
        help="Scratch directory (default: temporary)",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=None,
        help="Report output directory (default: <repo>/artifacts/librofm-apk-probe)",
    )
    parser.add_argument(
        "--apk",
        type=Path,
        default=None,
        help="Use an existing .apk/.xapk instead of downloading",
    )
    parser.add_argument("--package", default=PACKAGE_NAME)
    parser.add_argument("--apkeep-version", default=DEFAULT_APKEEP_VERSION)
    parser.add_argument("--jadx-version", default=DEFAULT_JADX_VERSION)
    parser.add_argument(
        "--fail-on-drift",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Exit 1 when tracked client.rs constants differ from the APK",
    )
    args = parser.parse_args(argv)

    repo = args.repo_root.resolve()
    client_rs = repo / "crates" / "bookclerk-libro" / "src" / "client.rs"
    if not client_rs.exists():
        log(f"error: missing {client_rs}")
        return 2

    out_dir = (args.out_dir or (repo / "artifacts" / "librofm-apk-probe")).resolve()
    keep_work = args.workdir is not None
    work = (args.workdir or Path(tempfile.mkdtemp(prefix="librofm-apk-"))).resolve()
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
            package_path = fetch_apk(apkeep, work / "download", args.package)

        base_apk, meta = resolve_base_apk(package_path, work)
        log(f"Base APK: {base_apk} ({base_apk.stat().st_size} bytes)")

        jadx = ensure_jadx(tools, args.jadx_version)
        sources = decompile(jadx, base_apk, work / "jadx-out")
        surface = extract_surface(sources, meta)
        client = parse_client_rs(client_rs)
        expected_shapes = load_expected_shapes(repo)
        drifts = compare_with_schema(surface, client, expected_shapes)
        write_outputs(out_dir, surface, client, drifts, meta)

        log(f"Wrote {out_dir / 'report.md'}")
        log(f"Wrote {out_dir / 'report.json'}")
        blocking = [d for d in drifts if d["severity"] == "error"]
        if blocking:
            log(f"Detected {len(blocking)} tracked drift(s):")
            for d in blocking:
                log(f"  - {d['field']}: apk={d['apk']!r} client={d['client']!r}")
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
