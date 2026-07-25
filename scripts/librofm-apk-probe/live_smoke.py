#!/usr/bin/env python3
"""Live-smoke Libro.fm API calls used by bookclerk-libro (auth path).

Auth profiles (`current`, `apk`) exercise oauth, library, and authenticated
catalog metadata via ``explore/audiobook_details/{isbn}`` (works for ISBNs
**not** in the library — empty CI accounts are fine). When the account owns
at least one title, also hits packaged_m4b / download-manifest and downloads
**one** media asset to probe magic bytes / zip contents.

Optional `public` profile hits explore endpoints without auth (informational).

Credentials (auth profiles; first match wins):
  email:    TEST_LIBRO_EMAIL | TEST_LIBRO_USERNAME | TEST_LIBRO_USER |
            LIBRO_FM_USERNAME
  password: TEST_LIBRO_PASSWORD | BOOKCLERK_LIBRO_PASSWORD | LIBRO_FM_PASSWORD

Optional:
  TEST_LIBRO_ISBN — prefer this **library** ISBN for download/media
  TEST_LIBRO_CATALOG_ISBN — any catalog ISBN for auth metadata (unowned OK)
  TEST_LIBRO_REQUIRE_MEDIA — if set, fail when no owned title (default: allow
                              empty library / metadata-only)
  TEST_LIBRO_MAX_DOWNLOAD_BYTES — cap media download (default 100 MiB)
  TEST_LIBRO_DOWNLOAD_DIR — keep downloaded bytes on disk for inspection

Exit codes:
  0 — selected profiles succeeded
  1 — one or more profile calls / media probes failed
  2 — missing credentials / bad args
"""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))
from schema_extract import json_key_tree, top_level_keys  # noqa: E402

# Default: one full title up to 100 MiB (prefer short TEST_LIBRO_ISBN in CI).
DEFAULT_MAX_DOWNLOAD_BYTES = 100 * 1024 * 1024
# When the object is larger, still fetch this many bytes for magic sniffing.
PROBE_PREFIX_BYTES = 2 * 1024 * 1024


def first_env(*names: str) -> str | None:
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return None


@dataclass
class Profile:
    name: str
    base_url: str
    oauth_path: str
    library_path: str
    download_manifest_path: str
    packaged_m4b_path: str
    app_ver: str
    user_agent: str
    extra_headers: dict[str, str]
    manifest_extra_query: dict[str, str]


def load_client_profile(repo: Path) -> Profile:
    # Import constants by parsing client.rs (avoid compiling Rust for a smoke).
    text = (repo / "crates/bookclerk-libro/src/client.rs").read_text(encoding="utf-8")
    consts: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("pub const "):
            continue
        # pub const FOO: &str = "bar";
        try:
            name = line.split()[2].rstrip(":")
            value = line.split("=", 1)[1].strip().rstrip(";").strip().strip('"')
            consts[name] = value
        except IndexError:
            continue
    # Mirror crates/bookclerk-libro/src/client.rs AuthInterceptor-equivalent
    # headers and DownloadApi client_version query (not separate pub consts).
    return Profile(
        name="current-client",
        base_url=consts["DEFAULT_BASE_URL"],
        oauth_path=consts["OAUTH_TOKEN_PATH"],
        library_path=consts["LIBRARY_PATH"],
        download_manifest_path=consts["DOWNLOAD_MANIFEST_PATH"],
        packaged_m4b_path=consts["PACKAGED_M4B_PATH"],
        app_ver=consts["APP_VER"],
        user_agent=consts["USER_AGENT_VALUE"],
        extra_headers={
            "X-LibroFm-Device": "bookclerk",
            "X-LibroFm-OsVer": "Android 34",
        },
        manifest_extra_query={
            "client_version": consts["APP_VER"],
        },
    )


def load_apk_profile(report_path: Path) -> Profile:
    report = json.loads(report_path.read_text(encoding="utf-8"))
    apk = report["apk"]
    tracked = apk.get("absolute_tracked_paths") or {}
    # Prefer absolute tracked paths; if incomplete (partial APK parse), compose
    # from api_prefix + relative names, then fall back to client constants in
    # the report so smoke fails with a clear error instead of KeyError.
    prefix = (apk.get("api_prefix") or "").rstrip("/")
    client = report.get("client") or {}

    def path_for(rel: str, client_key: str) -> str:
        if tracked.get(rel):
            return tracked[rel]
        if prefix:
            return f"{prefix}/{rel}"
        if client.get(client_key):
            return client[client_key]
        raise ValueError(
            f"APK report missing path for {rel!r} (absolute_tracked_paths, "
            f"api_prefix, and client.{client_key} all empty) in {report_path}"
        )

    # Match Android AuthInterceptor prod headers (no Api-Key on prod).
    return Profile(
        name="apk-extracted",
        base_url=apk.get("base_url") or client.get("default_base_url") or "https://libro.fm",
        oauth_path=apk.get("oauth_token_path")
        or client.get("oauth_token_path")
        or "/oauth/token",
        library_path=path_for("library", "library_path"),
        download_manifest_path=path_for("download-manifest", "download_manifest_path"),
        packaged_m4b_path=path_for(
            "audiobooks/{isbn}/packaged_m4b", "packaged_m4b_path"
        ),
        app_ver=apk.get("version_name") or client.get("app_ver") or "",
        user_agent=apk.get("okhttp_user_agent")
        or client.get("user_agent_value")
        or "okhttp/4.12.0",
        extra_headers={
            "X-LibroFm-Device": "bookclerk-smoke",
            "X-LibroFm-OsVer": "Android 34",
        },
        # Android DownloadApi: client_version=appVer, format=null for ZIP / "m4b" for M4B.
        manifest_extra_query={
            "client_version": apk.get("version_name") or client.get("app_ver") or "",
        },
    )


def http_json(
    method: str,
    url: str,
    *,
    headers: dict[str, str],
    body: dict[str, Any] | None = None,
    timeout: float = 60.0,
) -> tuple[int, Any, str]:
    data = None
    req_headers = dict(headers)
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        req_headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=data, headers=req_headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            status = resp.getcode()
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        status = exc.code
    except Exception as exc:  # noqa: BLE001
        return 0, None, f"request error: {exc}"

    parsed: Any
    try:
        parsed = json.loads(raw) if raw.strip() else None
    except json.JSONDecodeError:
        parsed = None
    return status, parsed, raw[:500]


def profile_headers(profile: Profile, token: str | None = None) -> dict[str, str]:
    headers = {
        "Accept": "application/json",
        "User-Agent": profile.user_agent,
        "X-LibroFm-AppVer": profile.app_ver,
        **profile.extra_headers,
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def url_is_libro_host(url: str) -> bool:
    host = (urlparse(url).hostname or "").lower()
    return host == "libro.fm" or host.endswith(".libro.fm")


def download_headers(profile: Profile, token: str, url: str) -> dict[str, str]:
    """CDN fetches: auth only for libro.fm hosts (mirrors LibroClient::download_bytes)."""
    headers = {
        "User-Agent": profile.user_agent,
        "Accept": "*/*",
    }
    if url_is_libro_host(url):
        headers.update(profile_headers(profile, token))
        headers["Accept"] = "*/*"
    return headers


def sniff_media(data: bytes) -> dict[str, Any]:
    """Identify downloaded bytes as the media object bookclerk-libro expects."""
    out: dict[str, Any] = {
        "bytes": len(data),
        "kind": "unknown",
        "ok": False,
    }
    if len(data) < 4:
        out["error"] = "payload too small to sniff"
        return out

    # ZIP of MP3 parts (common download-manifest shape)
    if data[:2] == b"PK":
        out["kind"] = "zip"
        audio_names: list[str] = []
        try:
            with zipfile.ZipFile(io.BytesIO(data)) as zf:
                for info in zf.infolist():
                    if info.is_dir():
                        continue
                    name = Path(info.filename).name.lower()
                    if name.endswith(
                        (".mp3", ".m4a", ".m4b", ".aac", ".flac", ".ogg")
                    ):
                        audio_names.append(info.filename)
                out["zip_entries"] = len(zf.infolist())
        except zipfile.BadZipFile as exc:
            out["error"] = f"zip magic but not a valid archive: {exc}"
            return out
        out["audio_entries"] = audio_names[:20]
        out["audio_entry_count"] = len(audio_names)
        out["ok"] = bool(audio_names)
        if not audio_names:
            out["error"] = "zip contained no audio files"
        return out

    # MPEG-4 / M4B / M4A (ftyp box at offset 4)
    if len(data) > 8 and data[4:8] == b"ftyp":
        brand = data[8:12].decode("ascii", errors="replace")
        # Libro packaged_m4b often brands as M4A_/M4B_ — both are valid audiobook containers.
        brand_u = brand.upper().replace("\x00", "")
        if brand_u.startswith("M4B") or b"M4B" in data[8:32]:
            out["kind"] = "m4b"
        elif brand_u.startswith("M4A") or b"M4A" in data[8:32]:
            out["kind"] = "m4a"
        else:
            out["kind"] = "mp4"
        out["ftyp_brand"] = brand
        out["ok"] = True
        return out

    # MP3
    if data.startswith(b"ID3") or (data[0] == 0xFF and (data[1] & 0xE0) == 0xE0):
        out["kind"] = "mp3"
        out["ok"] = True
        return out

    out["error"] = "unrecognized media magic"
    out["head_hex"] = data[:16].hex()
    return out


def http_download(
    url: str,
    *,
    headers: dict[str, str],
    max_bytes: int,
    timeout: float = 300.0,
) -> dict[str, Any]:
    """Download up to max_bytes from url; return status, body, headers metadata."""
    req = urllib.request.Request(url, headers=headers, method="GET")
    out: dict[str, Any] = {"url": url.split("?", 1)[0], "ok": False}
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status = resp.getcode()
            content_type = resp.headers.get("Content-Type")
            content_length = resp.headers.get("Content-Length")
            declared = int(content_length) if content_length and content_length.isdigit() else None
            chunks: list[bytes] = []
            total = 0
            truncated = False
            while True:
                chunk = resp.read(min(1024 * 256, max_bytes - total + 1))
                if not chunk:
                    break
                if total + len(chunk) > max_bytes:
                    chunks.append(chunk[: max_bytes - total])
                    total = max_bytes
                    truncated = True
                    break
                chunks.append(chunk)
                total += len(chunk)
            body = b"".join(chunks)
    except urllib.error.HTTPError as exc:
        out["status"] = exc.code
        out["error"] = exc.read()[:300].decode("utf-8", errors="replace")
        return out
    except Exception as exc:  # noqa: BLE001
        out["error"] = f"request error: {exc}"
        return out

    out["status"] = status
    out["content_type"] = content_type
    out["content_length"] = declared
    out["downloaded_bytes"] = len(body)
    out["truncated"] = truncated or (declared is not None and declared > len(body))
    out["body"] = body
    out["ok"] = status == 200 and len(body) > 0
    return out


def download_and_probe_media(
    profile: Profile,
    token: str,
    *,
    m4b_url: str | None,
    part_url: str | None,
    max_bytes: int,
    keep_dir: Path | None,
) -> dict[str, Any]:
    """Download one media URL (M4B preferred) and probe the object shape."""
    step: dict[str, Any] = {"name": "media_download_probe", "ok": False}
    url = m4b_url or part_url
    if not url:
        step["error"] = "no m4b_url or manifest part url to download"
        return step
    step["source"] = "packaged_m4b" if m4b_url else "manifest_part"
    step["max_bytes"] = max_bytes

    # Prefer full object when Content-Length is known and under cap; otherwise
    # fetch up to max_bytes (or PROBE_PREFIX for oversized titles).
    headers = download_headers(profile, token, url)
    # HEAD first when possible to decide cap.
    fetch_cap = max_bytes
    try:
        head_req = urllib.request.Request(
            url, headers=download_headers(profile, token, url), method="HEAD"
        )
        with urllib.request.urlopen(head_req, timeout=60) as head_resp:
            cl = head_resp.headers.get("Content-Length")
            if cl and cl.isdigit():
                size = int(cl)
                step["declared_content_length"] = size
                if size > max_bytes:
                    fetch_cap = min(PROBE_PREFIX_BYTES, max_bytes)
                    step["note"] = (
                        f"object is {size} bytes (> max {max_bytes}); "
                        f"probing first {fetch_cap} bytes only — set "
                        "TEST_LIBRO_ISBN to a shorter title for a full download"
                    )
    except Exception:  # noqa: BLE001
        # Some CDNs reject HEAD; fall through to GET with max_bytes.
        pass

    dl = http_download(url, headers=headers, max_bytes=fetch_cap)
    step["status"] = dl.get("status")
    step["content_type"] = dl.get("content_type")
    step["downloaded_bytes"] = dl.get("downloaded_bytes")
    step["truncated"] = dl.get("truncated")
    if not dl.get("ok"):
        step["error"] = dl.get("error") or f"download failed ({dl.get('status')})"
        return step

    body: bytes = dl["body"]
    probe = sniff_media(body)
    step["probe"] = {k: v for k, v in probe.items() if k != "body"}
    step["ok"] = bool(probe.get("ok"))
    if not step["ok"]:
        step["error"] = probe.get("error") or "media probe failed"

    if keep_dir is not None:
        keep_dir.mkdir(parents=True, exist_ok=True)
        ext = probe.get("kind") if probe.get("kind") != "unknown" else "bin"
        path = keep_dir / f"smoke-asset.{ext}"
        path.write_bytes(body)
        step["saved_to"] = str(path)

    # Drop body from return payload (too large for JSON reports).
    return step


def load_apk_shapes(report_path: Path) -> dict[str, Any]:
    report = json.loads(report_path.read_text(encoding="utf-8"))
    return (report.get("apk") or {}).get("tracked_shapes") or {}


def load_expected_shapes(repo: Path) -> dict[str, Any]:
    path = repo / "scripts" / "librofm-apk-probe" / "expected_shapes.json"
    if not path.exists():
        return {}
    data = json.loads(path.read_text(encoding="utf-8"))
    return {k: v for k, v in data.items() if isinstance(v, dict) and not k.startswith("_")}


def resolve_shapes(report: Path, repo: Path) -> dict[str, Any]:
    """Prefer APK-extracted shapes from the probe report; else committed expected."""
    if report.exists():
        shapes = load_apk_shapes(report)
        if shapes:
            return shapes
    return load_expected_shapes(repo)


def compare_live_to_apk(
    step_name: str,
    live_body: Any,
    apk_shape: dict[str, Any] | None,
) -> dict[str, Any]:
    """Compare live JSON keys to APK-declared response fields (incl. nested children)."""
    out: dict[str, Any] = {"step": step_name}
    if not isinstance(live_body, dict):
        out["ok"] = False
        out["error"] = "live body is not a JSON object"
        return out
    live_keys = set(top_level_keys(live_body))
    out["live_keys"] = sorted(live_keys)
    out["live_key_tree"] = json_key_tree(live_body, max_depth=2)
    if not apk_shape:
        out["ok"] = True
        out["note"] = "no APK shape to compare"
        return out
    expected = set(apk_shape.get("response_json_fields") or [])
    out["apk_response_fields"] = sorted(expected)
    missing = sorted(expected - live_keys)
    extra = sorted(live_keys - expected)
    out["missing_vs_apk"] = missing
    out["extra_vs_apk"] = extra
    # Nested arrays/objects: compare first element's keys to APK child DTO fields.
    nested_checks: list[dict[str, Any]] = []
    children = apk_shape.get("response_children") or {}
    if isinstance(children, dict):
        for child_key, child in children.items():
            if not isinstance(child, dict):
                continue
            raw_fields = child.get("json_fields")
            if isinstance(raw_fields, dict):
                apk_fields = set(raw_fields.keys())
            elif isinstance(raw_fields, list):
                apk_fields = set(raw_fields)
            else:
                apk_fields = set()
            sample = live_body.get(child_key)
            if isinstance(sample, list) and sample and isinstance(sample[0], dict):
                live_child_keys = set(sample[0].keys())
            elif isinstance(sample, dict):
                live_child_keys = set(sample.keys())
            else:
                continue
            nested_checks.append(
                {
                    "key": child_key,
                    "apk_fields": sorted(apk_fields),
                    "live_keys": sorted(live_child_keys),
                    "missing_vs_apk": sorted(apk_fields - live_child_keys),
                    "extra_vs_apk": sorted(live_child_keys - apk_fields),
                    "ok": bool(not apk_fields or (live_child_keys & apk_fields)),
                }
            )
    if nested_checks:
        out["nested"] = nested_checks
    # Missing declared fields are warnings: APK models may include optional keys.
    # Fail only when we got zero overlap on a non-empty expected set (clear mismatch).
    top_ok = True
    if expected and not (live_keys & expected):
        top_ok = False
        out["error"] = "live response shares no keys with APK-declared response fields"
    nested_ok = all(n.get("ok") for n in nested_checks) if nested_checks else True
    if not nested_ok and top_ok:
        out["error"] = "live nested object shares no keys with APK-declared child fields"
    out["ok"] = top_ok and nested_ok
    return out


_API_PREFIX_RE = re.compile(r"^(/api/v\d+/)")


def api_prefix_from_library_path(library_path: str) -> str:
    """Derive `/api/vN/` from a library path like `/api/v13/library`.

    Never hardcodes an API major version — bumps (`v12` → `v13`, …) are
    exactly what the APK probe is meant to detect and propagate into client
    constants and smoke URLs.
    """
    path = (library_path or "").strip()
    m = _API_PREFIX_RE.match(path)
    if m:
        return m.group(1)
    marker = "library"
    if path.endswith("/" + marker):
        return path[: -len(marker)]
    if path.endswith("/" + marker + "/"):
        return path[: -len(marker) - 1]
    if path.endswith(marker) and "/" in path:
        return path[: -len(marker)]
    raise ValueError(
        f"cannot derive /api/vN/ prefix from library path {library_path!r}; "
        "expected something like /api/vN/library from the APK or client constants"
    )


def run_profile(
    profile: Profile,
    email: str,
    password: str,
    preferred_isbn: str | None,
    apk_shapes: dict[str, Any] | None = None,
    *,
    download_media: bool = True,
    max_download_bytes: int = DEFAULT_MAX_DOWNLOAD_BYTES,
    download_dir: Path | None = None,
    catalog_isbn: str | None = None,
    require_media: bool = False,
) -> dict[str, Any]:
    """Auth smoke for acquire-adjacent APIs.

    Metadata for an ISBN you do **not** own is available via authenticated
    ``explore/audiobook_details/{isbn}`` (``purchase_info.owned=false``).
    ``download-manifest`` / ``packaged_m4b`` / CDN media require ownership
    (404 otherwise) — so an empty-library account can still validate auth +
    catalog metadata shapes; media probe runs only when a library title exists.
    """
    result: dict[str, Any] = {
        "profile": profile.name,
        "ok": False,
        "auth": True,
        "steps": [],
        "schema_checks": [],
    }
    base = profile.base_url.rstrip("/")
    prefix = api_prefix_from_library_path(profile.library_path)
    apk_shapes = apk_shapes or {}
    m4b_url: str | None = None
    part_url: str | None = None

    # 1) OAuth
    status, parsed, raw = http_json(
        "POST",
        f"{base}{profile.oauth_path}",
        headers=profile_headers(profile),
        body={
            "grant_type": "password",
            "username": email,
            "password": password,
        },
    )
    step = {"name": "oauth", "status": status}
    if status != 200 or not isinstance(parsed, dict) or not parsed.get("access_token"):
        step["error"] = raw
        result["steps"].append(step)
        return result
    token = parsed["access_token"]
    step["ok"] = True
    result["steps"].append(step)
    result["schema_checks"].append(
        compare_live_to_apk("oauth", parsed, apk_shapes.get("oauth/token"))
    )

    # 2) Library page 1 (may be empty for a dedicated CI account)
    status, parsed, raw = http_json(
        "GET",
        f"{base}{profile.library_path}?page=1",
        headers=profile_headers(profile, token),
    )
    step = {"name": "library", "status": status, "path": profile.library_path}
    if status != 200 or not isinstance(parsed, dict):
        step["error"] = raw
        result["steps"].append(step)
        return result
    books = parsed.get("audiobooks") or []
    owned_isbns = [str(b.get("isbn")) for b in books if b.get("isbn") is not None]
    step["ok"] = True
    step["book_count"] = len(books)
    step["total_pages"] = parsed.get("total_pages")
    step["empty_library"] = len(books) == 0
    result["steps"].append(step)
    result["schema_checks"].append(
        compare_live_to_apk("library", parsed, apk_shapes.get("library"))
    )

    # Catalog ISBN for unowned metadata (works with empty library).
    meta_isbn = (
        catalog_isbn
        or preferred_isbn
        or (owned_isbns[0] if owned_isbns else None)
        or "9780307749703"  # Foundation — stable catalog title
    )
    result["catalog_isbn"] = meta_isbn

    # 3) Authenticated explore details — works for ISBNs not in the library
    details_path = f"{prefix}explore/audiobook_details/{meta_isbn}"
    status, parsed, raw = http_json(
        "GET",
        f"{base}{details_path}",
        headers=profile_headers(profile, token),
    )
    step = {
        "name": "explore_audiobook_details_auth",
        "status": status,
        "path": details_path,
        "isbn": meta_isbn,
    }
    if status != 200 or not isinstance(parsed, dict):
        step["error"] = raw
        result["steps"].append(step)
        return result
    data = parsed.get("data") or {}
    book = data.get("audiobook") if isinstance(data, dict) else None
    purchase = data.get("purchase_info") if isinstance(data, dict) else None
    ui = parsed.get("user_info") if isinstance(parsed.get("user_info"), dict) else {}
    step["ok"] = True
    step["signed_in"] = ui.get("signed_in")
    if isinstance(book, dict):
        step["title"] = book.get("title")
    if isinstance(purchase, dict):
        step["owned"] = purchase.get("owned")
        step["for_sale"] = purchase.get("for_sale")
    result["steps"].append(step)
    result["schema_checks"].append(
        compare_live_to_apk(
            "explore_audiobook_details_auth",
            parsed,
            apk_shapes.get("explore/audiobook_details/{isbn}"),
        )
    )
    if isinstance(book, dict) and book:
        result["schema_checks"].append(
            {
                "step": "explore_audiobook_object_auth",
                "ok": bool({"title", "isbn"} <= set(book.keys())),
                "live_keys": sorted(book.keys()),
                "owned": purchase.get("owned") if isinstance(purchase, dict) else None,
            }
        )

    # Owned ISBN for download/media (empty library → skip; metadata already covered).
    owned_isbn: str | None = None
    if preferred_isbn and preferred_isbn in owned_isbns:
        owned_isbn = preferred_isbn
    elif owned_isbns:
        owned_isbn = owned_isbns[0]
    result["owned_isbn"] = owned_isbn

    if not owned_isbn:
        result["steps"].append(
            {
                "name": "download_manifest_m4b",
                "ok": True,
                "skipped": True,
                "note": "no owned library ISBN — skipped",
            }
        )
        result["steps"].append(
            {
                "name": "packaged_m4b",
                "ok": True,
                "skipped": True,
                "note": "no owned library ISBN — download APIs 404 for unowned titles",
            }
        )
        result["steps"].append(
            {
                "name": "download_manifest",
                "ok": True,
                "skipped": True,
                "note": "no owned library ISBN — skipped",
            }
        )
        media_note = (
            "empty/unowned library: media probe skipped. "
            "Auth catalog metadata was checked via explore/audiobook_details. "
            "Own one title (or set TEST_LIBRO_ISBN to a library book) to probe CDN media."
        )
        if require_media:
            result["steps"].append(
                {
                    "name": "media_download_probe",
                    "ok": False,
                    "error": media_note,
                }
            )
            result["ok"] = False
            return result
        result["steps"].append(
            {
                "name": "media_download_probe",
                "ok": True,
                "skipped": True,
                "note": media_note,
            }
        )
        schema_ok = all(c.get("ok") for c in result["schema_checks"])
        result["ok"] = all(s.get("ok") for s in result["steps"]) and schema_ok
        result["note"] = "metadata-only (no owned ISBN for download/media)"
        if not schema_ok:
            result["schema_error"] = True
        return result

    result["isbn"] = owned_isbn

    m4b_url = None
    part_url = None

    # 4) download-manifest?format=m4b (Android MediaFormat.M4B — preferred)
    q_m4b = {"isbn": owned_isbn, "format": "m4b", **profile.manifest_extra_query}
    url = f"{base}{profile.download_manifest_path}?{urllib.parse.urlencode(q_m4b)}"
    status, parsed, raw = http_json(
        "GET",
        url,
        headers=profile_headers(profile, token),
    )
    step = {
        "name": "download_manifest_m4b",
        "status": status,
        "path": profile.download_manifest_path,
        "isbn": owned_isbn,
        "query": q_m4b,
    }
    if status == 200 and isinstance(parsed, dict):
        step["ok"] = True
        parts = parsed.get("parts") or []
        tracks = parsed.get("tracks") or []
        step["parts"] = len(parts)
        step["tracks"] = len(tracks)
        for part in parts:
            if not isinstance(part, dict):
                continue
            u = part.get("url")
            if isinstance(u, str) and u.lower().split("?", 1)[0].endswith(".m4b"):
                m4b_url = u
                step["has_m4b_part"] = True
                break
        result["schema_checks"].append(
            compare_live_to_apk(
                "download_manifest_m4b", parsed, apk_shapes.get("download-manifest")
            )
        )
    elif status in (404,) or (400 <= status < 500):
        # Some titles may reject format=m4b; ZIP/packaged_m4b still work.
        step["ok"] = True
        step["note"] = f"format=m4b returned {status}; falling back"
    else:
        step["error"] = raw
    result["steps"].append(step)
    if not step.get("ok"):
        return result

    # 5) Packaged M4B meta (ownership required; same CDN object when present)
    m4b_path = profile.packaged_m4b_path.replace("{isbn}", owned_isbn)
    status, parsed, raw = http_json(
        "GET",
        f"{base}{m4b_path}",
        headers=profile_headers(profile, token),
    )
    step = {
        "name": "packaged_m4b",
        "status": status,
        "path": m4b_path,
        "isbn": owned_isbn,
    }
    if status in (200, 404) or (400 <= status < 500):
        step["ok"] = True
        if isinstance(parsed, dict):
            packaged_url = parsed.get("m4b_url") or None
            if isinstance(packaged_url, str) and not packaged_url.strip():
                packaged_url = None
            step["has_m4b_url"] = bool(packaged_url)
            if m4b_url is None and packaged_url:
                m4b_url = packaged_url
            if status == 200:
                result["schema_checks"].append(
                    compare_live_to_apk(
                        "packaged_m4b",
                        parsed,
                        apk_shapes.get("audiobooks/{isbn}/packaged_m4b"),
                    )
                )
    else:
        step["error"] = raw
    result["steps"].append(step)
    if not step.get("ok"):
        return result

    # 6) Download manifest without format → ZIP parts (MediaFormat.ZIP)
    q = {"isbn": owned_isbn, **profile.manifest_extra_query}
    url = f"{base}{profile.download_manifest_path}?{urllib.parse.urlencode(q)}"
    status, parsed, raw = http_json(
        "GET",
        url,
        headers=profile_headers(profile, token),
    )
    step = {
        "name": "download_manifest",
        "status": status,
        "path": profile.download_manifest_path,
        "isbn": owned_isbn,
        "query": q,
    }
    if status != 200 or not isinstance(parsed, dict):
        step["error"] = raw
        result["steps"].append(step)
        return result
    parts = parsed.get("parts") or []
    tracks = parsed.get("tracks") or []
    step["ok"] = True
    step["parts"] = len(parts)
    step["tracks"] = len(tracks)
    if parts and isinstance(parts[0], dict):
        part_url = parts[0].get("url")
    result["steps"].append(step)
    result["schema_checks"].append(
        compare_live_to_apk(
            "download_manifest", parsed, apk_shapes.get("download-manifest")
        )
    )

    # 7) Download one media asset and probe magic / zip audio entries
    if download_media:
        media_step = download_and_probe_media(
            profile,
            token,
            m4b_url=m4b_url,
            part_url=part_url if isinstance(part_url, str) else None,
            max_bytes=max_download_bytes,
            keep_dir=download_dir,
        )
        # Prefer labeling format=m4b when that supplied the URL.
        if m4b_url and media_step.get("source") == "packaged_m4b":
            # download_and_probe_media labels any m4b_url as packaged_m4b;
            # refine when the URL came from the format=m4b manifest.
            for prev in result["steps"]:
                if prev.get("name") == "download_manifest_m4b" and prev.get(
                    "has_m4b_part"
                ):
                    media_step["source"] = "download_manifest_m4b"
                    break
        result["steps"].append(media_step)
        if not media_step.get("ok"):
            result["ok"] = False
            return result
    else:
        result["steps"].append(
            {
                "name": "media_download_probe",
                "ok": True,
                "skipped": True,
                "note": "media download disabled (--no-media-download or already probed)",
            }
        )

    schema_ok = all(c.get("ok") for c in result["schema_checks"])
    result["ok"] = all(s.get("ok") for s in result["steps"]) and schema_ok
    if not schema_ok:
        result["schema_error"] = True
    return result


def run_public_smoke(
    profile: Profile,
    *,
    apk_shapes: dict[str, Any] | None = None,
    search_query: str = "Foundation",
    preferred_isbn: str | None = None,
) -> dict[str, Any]:
    """Hit public explore endpoints (no Authorization) and compare shapes.

    Catalog metadata is available without login for any ISBN in the store;
    library / download-manifest / packaged_m4b remain auth-only (401).
    """
    result: dict[str, Any] = {
        "profile": f"{profile.name}-public",
        "ok": False,
        "auth": False,
        "steps": [],
        "schema_checks": [],
    }
    base = profile.base_url.rstrip("/")
    prefix = api_prefix_from_library_path(profile.library_path)
    headers = profile_headers(profile, token=None)
    apk_shapes = apk_shapes or {}

    # 1) Search (QueryMap: q=…)
    search_path = f"{prefix}explore/search"
    q = {"q": search_query, "page": "1"}
    status, parsed, raw = http_json(
        "GET",
        f"{base}{search_path}?{urllib.parse.urlencode(q)}",
        headers=headers,
    )
    step = {"name": "explore_search", "status": status, "path": search_path, "query": q}
    if status != 200 or not isinstance(parsed, dict):
        step["error"] = raw
        result["steps"].append(step)
        return result
    collection = parsed.get("audiobook_collection") or {}
    books = collection.get("audiobooks") or []
    step["ok"] = True
    step["book_count"] = len(books)
    step["total_pages"] = collection.get("total_pages")
    step["signed_in"] = (parsed.get("user_info") or {}).get("signed_in")
    result["steps"].append(step)
    result["schema_checks"].append(
        compare_live_to_apk("explore_search", parsed, apk_shapes.get("explore/search"))
    )

    isbn = preferred_isbn
    if not isbn and books:
        isbn = str(books[0].get("isbn") or "")
    if not isbn:
        result["ok"] = True
        result["note"] = "search empty; skipped details"
        return result
    result["isbn"] = isbn

    # 2) Suggest
    suggest_path = f"{prefix}explore/search/suggest"
    status, parsed, raw = http_json(
        "GET",
        f"{base}{suggest_path}?{urllib.parse.urlencode({'q': search_query[:5]})}",
        headers=headers,
    )
    step = {"name": "explore_suggest", "status": status, "path": suggest_path}
    if status == 200 and isinstance(parsed, dict):
        step["ok"] = True
        result["schema_checks"].append(
            compare_live_to_apk(
                "explore_suggest", parsed, apk_shapes.get("explore/search/suggest")
            )
        )
    else:
        step["ok"] = False
        step["error"] = raw
    result["steps"].append(step)
    if not step.get("ok"):
        return result

    # 3) Genres
    genres_path = f"{prefix}explore/genres"
    status, parsed, raw = http_json("GET", f"{base}{genres_path}", headers=headers)
    step = {"name": "explore_genres", "status": status, "path": genres_path}
    if status == 200 and isinstance(parsed, dict):
        step["ok"] = True
        result["schema_checks"].append(
            compare_live_to_apk(
                "explore_genres", parsed, apk_shapes.get("explore/genres")
            )
        )
    else:
        step["ok"] = False
        step["error"] = raw
    result["steps"].append(step)
    if not step.get("ok"):
        return result

    # 4) Audiobook details for catalog ISBN (not necessarily in a library)
    details_path = f"{prefix}explore/audiobook_details/{isbn}"
    status, parsed, raw = http_json("GET", f"{base}{details_path}", headers=headers)
    step = {
        "name": "explore_audiobook_details",
        "status": status,
        "path": details_path,
        "isbn": isbn,
    }
    if status != 200 or not isinstance(parsed, dict):
        step["error"] = raw
        result["steps"].append(step)
        return result
    data = parsed.get("data") or {}
    book = data.get("audiobook") or {}
    step["ok"] = True
    step["title"] = book.get("title")
    step["signed_in"] = (parsed.get("user_info") or {}).get("signed_in")
    result["steps"].append(step)
    result["schema_checks"].append(
        compare_live_to_apk(
            "explore_audiobook_details",
            parsed,
            apk_shapes.get("explore/audiobook_details/{isbn}"),
        )
    )
    # Extra: catalog audiobook object keys (shared with library rows).
    if isinstance(book, dict) and book:
        result["schema_checks"].append(
            {
                "step": "explore_audiobook_object",
                "ok": bool({"title", "isbn"} <= set(book.keys())),
                "live_keys": sorted(book.keys()),
                "note": "public catalog book shape; library rows are richer but overlap",
            }
        )

    schema_ok = all(c.get("ok") for c in result["schema_checks"])
    result["ok"] = all(s.get("ok") for s in result["steps"]) and schema_ok
    if not schema_ok:
        result["schema_error"] = True
    return result


def _run_self_test() -> int:
    """Local checks that do not hit the network."""
    assert api_prefix_from_library_path("/api/v12/library") == "/api/v12/"
    assert api_prefix_from_library_path("/api/v13/library") == "/api/v13/"
    assert api_prefix_from_library_path("/api/v99/library/") == "/api/v99/"
    assert api_prefix_from_library_path("/api/v7/library") == "/api/v7/"
    # Prefix-only paths still yield /api/vN/ (explore URLs use this).
    assert api_prefix_from_library_path("/api/v15/") == "/api/v15/"
    try:
        api_prefix_from_library_path("library")
        raise AssertionError("expected ValueError for bare library path")
    except ValueError:
        pass
    try:
        api_prefix_from_library_path("/something/else")
        raise AssertionError("expected ValueError for non-/api/vN/ path")
    except ValueError:
        pass

    # Incomplete absolute_tracked_paths must fall back to api_prefix, not KeyError.
    with tempfile.TemporaryDirectory() as td:
        report = Path(td) / "report.json"
        report.write_text(
            json.dumps(
                {
                    "apk": {
                        "base_url": "https://libro.fm",
                        "api_prefix": "/api/v13/",
                        "version_name": "9.0.0",
                        "okhttp_user_agent": "okhttp/4.12.0",
                        "oauth_token_path": "/oauth/token",
                        "absolute_tracked_paths": {},
                    },
                    "client": {},
                }
            ),
            encoding="utf-8",
        )
        profile = load_apk_profile(report)
        assert profile.library_path == "/api/v13/library"
        assert profile.download_manifest_path == "/api/v13/download-manifest"
        assert "v13" in profile.packaged_m4b_path

    print("live_smoke self-test ok")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run local unit checks (API prefix derivation) and exit",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=None,
        help="APK probe report.json (enables apk-extracted profile)",
    )
    parser.add_argument(
        "--profiles",
        default="current,apk",
        help="Comma list: current, apk, public",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=None,
        help="Write JSON results here",
    )
    parser.add_argument(
        "--search-query",
        default="Foundation",
        help="Catalog search query for public smoke (default: Foundation)",
    )
    parser.add_argument(
        "--catalog-isbn",
        default=None,
        help="Catalog ISBN for auth explore/details (unowned OK; env TEST_LIBRO_CATALOG_ISBN)",
    )
    parser.add_argument(
        "--max-download-bytes",
        type=int,
        default=None,
        help="Cap for media download (default env TEST_LIBRO_MAX_DOWNLOAD_BYTES or 100MiB)",
    )
    parser.add_argument(
        "--no-media-download",
        action="store_true",
        help="Skip CDN media download/probe (API JSON only)",
    )
    parser.add_argument(
        "--require-media",
        action="store_true",
        help="Fail if no owned library ISBN (env TEST_LIBRO_REQUIRE_MEDIA)",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return _run_self_test()

    email = first_env(
        "TEST_LIBRO_EMAIL",
        "TEST_LIBRO_USERNAME",
        "TEST_LIBRO_USER",
        "LIBRO_FM_USERNAME",
    )
    password = first_env(
        "TEST_LIBRO_PASSWORD", "BOOKCLERK_LIBRO_PASSWORD", "LIBRO_FM_PASSWORD"
    )
    isbn = first_env("TEST_LIBRO_ISBN")
    catalog_isbn = args.catalog_isbn or first_env("TEST_LIBRO_CATALOG_ISBN")
    require_media = args.require_media or bool(first_env("TEST_LIBRO_REQUIRE_MEDIA"))
    wanted = {p.strip() for p in args.profiles.split(",") if p.strip()}
    auth_wanted = bool(wanted & {"current", "apk"})
    public_wanted = "public" in wanted

    if auth_wanted and (not email or not password):
        print(
            "error: missing Libro credentials for auth profiles. Set TEST_LIBRO_EMAIL "
            "(or TEST_LIBRO_USERNAME / TEST_LIBRO_USER) and TEST_LIBRO_PASSWORD.",
            file=sys.stderr,
        )
        return 2

    max_bytes = args.max_download_bytes
    if max_bytes is None:
        env_max = first_env("TEST_LIBRO_MAX_DOWNLOAD_BYTES")
        max_bytes = int(env_max) if env_max and env_max.isdigit() else DEFAULT_MAX_DOWNLOAD_BYTES
    keep_raw = first_env("TEST_LIBRO_DOWNLOAD_DIR")
    download_dir = Path(keep_raw) if keep_raw else None

    report = args.report or (args.repo_root / "artifacts/librofm-apk-probe/report.json")
    apk_shapes = resolve_shapes(report, args.repo_root)

    results: list[dict[str, Any]] = []

    if public_wanted:
        if (args.repo_root / "crates/bookclerk-libro/src/client.rs").exists():
            pub_profile = load_client_profile(args.repo_root)
        elif report.exists():
            pub_profile = load_apk_profile(report)
        else:
            print("error: need client.rs or APK report for public smoke", file=sys.stderr)
            return 2
        results.append(
            run_public_smoke(
                pub_profile,
                apk_shapes=apk_shapes,
                search_query=args.search_query,
                preferred_isbn=catalog_isbn or isbn,
            )
        )

    auth_profiles: list[Profile] = []
    if "current" in wanted:
        auth_profiles.append(load_client_profile(args.repo_root))
    if "apk" in wanted:
        if not report.exists():
            print(f"error: APK report not found: {report}", file=sys.stderr)
            return 2
        try:
            auth_profiles.append(load_apk_profile(report))
        except (KeyError, ValueError, TypeError) as exc:
            print(f"error: cannot build apk-extracted profile: {exc}", file=sys.stderr)
            return 2
    for i, p in enumerate(auth_profiles):
        assert email and password
        # Download media once (CDN is path-constant-independent).
        do_media = (not args.no_media_download) and i == 0
        results.append(
            run_profile(
                p,
                email,
                password,
                isbn,
                apk_shapes=apk_shapes,
                download_media=do_media,
                max_download_bytes=max_bytes,
                download_dir=download_dir if do_media else None,
                catalog_isbn=catalog_isbn,
                require_media=require_media,
            )
        )

    payload = {
        "results": results,
        "email_present": bool(email),
        "isbn": isbn,
        "catalog_isbn": catalog_isbn,
        "apk_shapes_loaded": bool(apk_shapes),
        "modes": sorted(wanted),
        "max_download_bytes": max_bytes,
        "media_download": not args.no_media_download,
        "require_media": require_media,
    }
    text = json.dumps(payload, indent=2)
    print(text)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")

    if not results:
        return 2
    return 0 if all(r["ok"] for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
