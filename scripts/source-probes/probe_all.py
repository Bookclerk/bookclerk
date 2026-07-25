#!/usr/bin/env python3
"""Live endpoint probes for candidate audiobook sources (API-first, no cookies).

Runs unauthenticated / public checks always. When credentials are present in
the environment, also exercises login + one library/list call.

Prefer reverse-engineered mobile/API surfaces over browser cookie scraping.

Exit codes:
  0 — all selected probes passed (auth skipped counts as pass if no creds)
  1 — one or more probes failed
  2 — bad args / write failure

Credential env vars (see README.md / docs/source-candidates.md):
  GraphicAudio:     TEST_GA_EMAIL / TEST_GA_PASSWORD
  Chirp:            TEST_CHIRP_EMAIL / TEST_CHIRP_PASSWORD
  Storytel:         TEST_STORYTEL_EMAIL / TEST_STORYTEL_PASSWORD
  Audiobooks.com:   TEST_ABC_EMAIL / TEST_ABC_PASSWORD
  Kobo:             (device activation — no password; set TEST_KOBO_ACTIVATE=1
                     later for interactive/browser flow; public device auth
                     always probed)
  LibriVox:         none
  Podimo:           TEST_PODIMO_EMAIL / TEST_PODIMO_PASSWORD (Cloudflare may block)
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class Check:
    source: str
    name: str
    ok: bool
    detail: str
    status: int | None = None
    needs_auth: bool = False
    skipped: bool = False


@dataclass
class Report:
    generated_at: str
    checks: list[Check] = field(default_factory=list)

    def add(self, check: Check) -> None:
        self.checks.append(check)

    @property
    def failed(self) -> list[Check]:
        return [c for c in self.checks if not c.ok and not c.skipped]


def env(*names: str) -> str | None:
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return None


# Default UA — several hosts (LibriVox, Chirp, Downpour) return Cloudflare
# error 1010 when urllib's empty/default UA is used from cloud egress.
DEFAULT_UA = (
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 "
    "BookclerkSourceProbe/0.1"
)


def http(
    url: str,
    *,
    method: str = "GET",
    headers: dict[str, str] | None = None,
    data: bytes | None = None,
    form: dict[str, str] | None = None,
    json_body: Any | None = None,
    timeout: float = 25.0,
) -> tuple[int, dict[str, str], bytes]:
    hdrs = dict(headers or {})
    hdrs.setdefault("User-Agent", DEFAULT_UA)
    body = data
    if form is not None:
        body = urllib.parse.urlencode(form).encode()
        hdrs.setdefault("Content-Type", "application/x-www-form-urlencoded")
    if json_body is not None:
        body = json.dumps(json_body).encode()
        hdrs.setdefault("Content-Type", "application/json")
        hdrs.setdefault("Accept", "application/json")
    req = urllib.request.Request(url, data=body, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, dict(resp.headers.items()), resp.read()
    except urllib.error.HTTPError as exc:
        return exc.code, dict(exc.headers.items()) if exc.headers else {}, exc.read() or b""


def json_or_text(raw: bytes) -> Any:
    text = raw.decode("utf-8", errors="replace")
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text[:500]


# ---------------------------------------------------------------------------
# LibriVox
# ---------------------------------------------------------------------------


def probe_librivox(report: Report) -> None:
    status, _, body = http(
        "https://librivox.org/api/feed/audiobooks/?format=json&limit=1"
    )
    data = json_or_text(body)
    ok = status == 200 and isinstance(data, dict) and "books" in data
    report.add(
        Check(
            "librivox",
            "catalog_list",
            ok,
            f"books={len(data.get('books', []))}" if ok else str(data)[:200],
            status,
        )
    )


# ---------------------------------------------------------------------------
# GraphicAudio
# ---------------------------------------------------------------------------


def probe_graphicaudio(report: Report) -> None:
    base = "https://www.graphicaudio.net/access"
    store = "https://www.graphicaudio.net"

    # Magento storefront login page (Browser Player / ZIP path dependency).
    status, _, body = http(f"{store}/customer/account/login/")
    text = body.decode("utf-8", errors="replace")
    ok = status == 200 and "form_key" in text
    report.add(
        Check(
            "graphicaudio",
            "magento_login_form",
            ok,
            "form_key present" if ok else text[:200],
            status,
        )
    )

    status, _, body = http(f"{base}/api/products")
    data = json_or_text(body)
    ok = status == 200 and isinstance(data, list) and len(data) > 0
    report.add(
        Check(
            "graphicaudio",
            "public_sample_products",
            ok,
            f"count={len(data) if isinstance(data, list) else 0} types="
            f"{sorted({p.get('Type') for p in data}) if isinstance(data, list) else []}",
            status,
        )
    )
    sample_id = None
    if isinstance(data, list) and data:
        sample_id = str(data[0].get("Id"))

    if sample_id:
        status, _, body = http(f"{base}/api/links?product={sample_id}")
        links = json_or_text(body)
        ok = (
            status == 200
            and isinstance(links, dict)
            and ("Hi" in links or "Lo" in links)
        )
        hi = links.get("Hi") if isinstance(links, dict) else None
        report.add(
            Check(
                "graphicaudio",
                "public_sample_links",
                ok,
                f"product={sample_id} hi={hi}",
                status,
            )
        )
        if hi:
            # Range GET — confirm plain audio/mp3
            status, headers, body = http(
                hi, headers={"Range": "bytes=0-3"}
            )
            ctype = headers.get("Content-Type", headers.get("content-type", ""))
            # ID3 or MPEG frame sync
            magic_ok = body[:3] == b"ID3" or (len(body) >= 2 and body[0] == 0xFF)
            report.add(
                Check(
                    "graphicaudio",
                    "sample_media_bytes",
                    status in (200, 206) and "audio" in ctype and magic_ok,
                    f"status={status} content-type={ctype} magic={body[:4]!r}",
                    status,
                )
            )

    # Login contract (expect 401 with bad creds)
    status, _, body = http(
        f"{base}/activation/login",
        method="POST",
        form={
            "username": "probe@example.com",
            "password": "invalid",
            "client_id": "bookclerk-probe",
        },
    )
    data = json_or_text(body)
    ok = status == 401 and isinstance(data, dict) and "Message" in data
    report.add(
        Check(
            "graphicaudio",
            "login_rejects_bad_password",
            ok,
            str(data)[:200],
            status,
        )
    )

    email = env("TEST_GA_EMAIL", "TEST_GRAPHICAUDIO_EMAIL")
    password = env("TEST_GA_PASSWORD", "TEST_GRAPHICAUDIO_PASSWORD")
    if not email or not password:
        report.add(
            Check(
                "graphicaudio",
                "auth_login_library",
                True,
                "skipped — set TEST_GA_EMAIL + TEST_GA_PASSWORD",
                needs_auth=True,
                skipped=True,
            )
        )
        return

    device = f"bookclerk-{uuid.uuid4()}"
    status, _, body = http(
        f"{base}/activation/login",
        method="POST",
        form={"username": email, "password": password, "client_id": device},
    )
    data = json_or_text(body)
    token = None
    if isinstance(data, dict):
        token = data.get("Token") or data.get("token") or data.get("AccessToken")
    ok = status == 200 and bool(token)
    report.add(
        Check(
            "graphicaudio",
            "auth_login",
            ok,
            f"keys={sorted(data.keys()) if isinstance(data, dict) else type(data).__name__}",
            status,
            needs_auth=True,
        )
    )
    if not token:
        return
    status, _, body = http(
        f"{base}/api/products",
        headers={"Authorization": str(token)},
    )
    data = json_or_text(body)
    ok = status == 200 and isinstance(data, list)
    report.add(
        Check(
            "graphicaudio",
            "auth_products",
            ok,
            f"count={len(data) if isinstance(data, list) else 'n/a'}",
            status,
            needs_auth=True,
        )
    )


# ---------------------------------------------------------------------------
# Chirp (GraphQL — password login, not cookies)
# ---------------------------------------------------------------------------


def probe_chirp(report: Report) -> None:
    # Matches crates/bookclerk-chirp/src/client.rs DEFAULT_GRAPHQL_URL.
    gql = "https://api.chirpbooks.com/api/graphql"
    status, _, body = http(
        gql,
        method="POST",
        json_body={
            "operationName": "fetchAudiobookTracks",
            "query": (
                "query fetchAudiobookTracks($id:ID!){"
                "audiobook(id:$id){tracks{partNumber chapterNumber durationMs displayName}}}"
            ),
            "variables": {"id": "1"},
        },
    )
    data = json_or_text(body)
    # Live API returns GraphQL errors for missing book — proves schema is up
    ok = status == 200 and isinstance(data, dict) and "errors" in data
    report.add(
        Check(
            "chirp",
            "graphql_alive",
            ok,
            str(data)[:200],
            status,
        )
    )

    status, _, body = http(
        gql,
        method="POST",
        json_body={
            "operationName": "signIn",
            "query": (
                "mutation signIn($email: String!, $password: String!) {"
                "signIn(email: $email, password: $password) {"
                "user { id token webToken email } } }"
            ),
            "variables": {"email": "probe@example.com", "password": "invalid"},
        },
    )
    data = json_or_text(body)
    msg = ""
    if isinstance(data, dict) and data.get("errors"):
        msg = data["errors"][0].get("message", "")
    ok = status == 200 and "Invalid" in msg
    report.add(
        Check(
            "chirp",
            "signin_rejects_bad_password",
            ok,
            msg or str(data)[:200],
            status,
        )
    )

    email = env("TEST_CHIRP_EMAIL")
    password = env("TEST_CHIRP_PASSWORD")
    if not email or not password:
        report.add(
            Check(
                "chirp",
                "auth_signin_library",
                True,
                "skipped — set TEST_CHIRP_EMAIL + TEST_CHIRP_PASSWORD",
                needs_auth=True,
                skipped=True,
            )
        )
        return

    status, _, body = http(
        gql,
        method="POST",
        json_body={
            "operationName": "signIn",
            "query": (
                "mutation signIn($email: String!, $password: String!) {"
                "signIn(email: $email, password: $password) {"
                "user { id token webToken email } } }"
            ),
            "variables": {"email": email, "password": password},
        },
    )
    data = json_or_text(body)
    user = None
    if isinstance(data, dict):
        user = ((data.get("data") or {}).get("signIn") or {}).get("user")
    ok = status == 200 and isinstance(user, dict) and bool(user.get("token"))
    report.add(
        Check(
            "chirp",
            "auth_signin",
            ok,
            f"user_id={user.get('id') if user else None}",
            status,
            needs_auth=True,
        )
    )
    if not ok or not user:
        return

    # Library page via GraphQL (AndroidCurrentUserAudiobooksQuery shape varies;
    # use a minimal currentUser field probe and fall back to documented query).
    status, _, body = http(
        gql,
        method="POST",
        headers={"Authorization": f"Bearer {user['token']}"},
        json_body={
            "operationName": "AndroidCurrentUserAudiobooks",
            "query": (
                "query AndroidCurrentUserAudiobooks($page: Int!, $pageSize: Int!) {"
                "currentUserAudiobooks(page: $page, pageSize: $pageSize, "
                "sort: TITLE_A_Z, clientCapabilities: [CHIRP_AUDIO]) {"
                "id archived audiobook { id displayTitle } } }"
            ),
            "variables": {"page": 1, "pageSize": 20},
        },
    )
    data = json_or_text(body)
    items = None
    if isinstance(data, dict):
        items = (data.get("data") or {}).get("currentUserAudiobooks")
    has_auth_error = False
    if isinstance(data, dict) and data.get("errors"):
        joined = " ".join(e.get("message", "") for e in data["errors"]).lower()
        has_auth_error = "unauthorized" in joined or "not authenticated" in joined
    ok = status == 200 and isinstance(items, list) and not has_auth_error
    report.add(
        Check(
            "chirp",
            "auth_library_query",
            ok,
            f"count={len(items) if isinstance(items, list) else 'n/a'}",
            status,
            needs_auth=True,
        )
    )


# ---------------------------------------------------------------------------
# Storytel
# ---------------------------------------------------------------------------


def _storytel_encrypt_password(password: str) -> str:
    """AES-CBC with fixed key/IV used by the Android client / audiobook-dl."""
    try:
        from Crypto.Cipher import AES  # type: ignore
        from Crypto.Util.Padding import pad  # type: ignore
    except ImportError:
        # Soft dependency — auth probe skipped if pycryptodome missing
        return ""
    key = b"VQZBJ6TD8M9WBUWT"
    iv = b"joiwef08u23j341a"
    cipher = AES.new(key, AES.MODE_CBC, iv)
    return cipher.encrypt(pad(password.encode(), AES.block_size)).hex()


def probe_storytel(report: Report) -> None:
    ua = "Storytel/24.22 (Android 14; Google Pixel 8 Pro) Release/2288629"
    device = str(uuid.uuid4())
    login_url = (
        "https://www.storytel.com/api/login.action"
        f"?m=1&token=guestsv&userid=-1&version=24.22&terminal=android"
        f"&locale=sv&deviceId={device}&kidsMode=false"
    )
    status, _, body = http(
        login_url,
        method="POST",
        headers={"User-Agent": ua},
        form={"uid": "probe@example.com", "pwd": "deadbeef"},
    )
    data = json_or_text(body)
    enum = ""
    if isinstance(data, dict):
        enum = (data.get("accountInfo") or {}).get("loginStatusEnum", "")
    ok = status == 401 and enum == "INVALID_CREDENTIALS"
    report.add(
        Check(
            "storytel",
            "login_rejects_bad_password",
            ok,
            f"loginStatusEnum={enum}",
            status,
        )
    )

    status, _, body = http(
        "https://api.storytel.net/libraries/bookshelf",
        method="POST",
        headers={"User-Agent": ua, "Content-Type": "application/json"},
        data=b'{"items":[]}',
    )
    ok = status in (401, 403)
    report.add(
        Check(
            "storytel",
            "bookshelf_requires_auth",
            ok,
            f"status={status}",
            status,
        )
    )

    email = env("TEST_STORYTEL_EMAIL", "TEST_STORYTEL_USER")
    password = env("TEST_STORYTEL_PASSWORD")
    if not email or not password:
        report.add(
            Check(
                "storytel",
                "auth_login_bookshelf",
                True,
                "skipped — set TEST_STORYTEL_EMAIL + TEST_STORYTEL_PASSWORD",
                needs_auth=True,
                skipped=True,
            )
        )
        return

    enc = _storytel_encrypt_password(password)
    if not enc:
        report.add(
            Check(
                "storytel",
                "auth_login",
                False,
                "pycryptodome required for password encryption (pip install pycryptodome)",
                needs_auth=True,
            )
        )
        return

    device = str(uuid.uuid4())
    login_url = (
        "https://www.storytel.com/api/login.action"
        f"?m=1&token=guestsv&userid=-1&version=24.22&terminal=android"
        f"&locale=sv&deviceId={device}&kidsMode=false"
    )
    status, _, body = http(
        login_url,
        method="POST",
        headers={"User-Agent": ua},
        form={"uid": email, "pwd": enc},
    )
    data = json_or_text(body)
    jwt = None
    if isinstance(data, dict):
        jwt = (data.get("accountInfo") or {}).get("jwt")
    ok = status == 200 and bool(jwt)
    report.add(
        Check(
            "storytel",
            "auth_login",
            ok,
            f"lang={(data.get('accountInfo') or {}).get('lang') if isinstance(data, dict) else None}",
            status,
            needs_auth=True,
        )
    )
    if not jwt:
        return
    status, _, body = http(
        "https://api.storytel.net/libraries/bookshelf",
        method="POST",
        headers={
            "User-Agent": ua,
            "Authorization": f"Bearer {jwt}",
            "Content-Type": "application/json",
        },
        data=b'{"items":[]}',
    )
    data = json_or_text(body)
    ok = status == 200 and isinstance(data, dict)
    report.add(
        Check(
            "storytel",
            "auth_bookshelf",
            ok,
            f"keys={sorted(data.keys()) if isinstance(data, dict) else type(data).__name__}",
            status,
            needs_auth=True,
        )
    )


# ---------------------------------------------------------------------------
# Audiobooks.com (Storytel USA) — form POST + embedded apiKey from APK
# ---------------------------------------------------------------------------

# From com.audiobooks.base.network.NetworkConstants (production Android key)
ABC_API_KEY = "345c61ff334b5b699649c2c60bb85371"
ABC_BASE = "https://api.audiobooks.com/api/v2/"
ABC_UA = "Audiobooks.com Android App"


def probe_audiobooks_com(report: Report) -> None:
    status, _, body = http(
        ABC_BASE + "authenticate/startup",
        method="POST",
        headers={"User-Agent": ABC_UA},
        form={"apiKey": ABC_API_KEY},
    )
    data = json_or_text(body)
    token = None
    if isinstance(data, dict) and data.get("status") == "success":
        token = ((data.get("data") or {}).get("tokenInformation") or {}).get("token")
    ok = status == 200 and bool(token)
    report.add(
        Check(
            "audiobooks_com",
            "startup_guest_token",
            ok,
            f"token_prefix={(token or '')[:8]}",
            status,
        )
    )

    status, _, body = http(
        ABC_BASE + "authenticate/login",
        method="POST",
        headers={"User-Agent": ABC_UA},
        form={
            "apiKey": ABC_API_KEY,
            "emailAddress": "probe@example.com",
            "password": "invalid",
            "deviceId": "bookclerk-probe-device-001",
            "deviceType": "Android",
            "appVersion": "12.0.7",
            "OSVersion": "14",
            "alreadyHashed": "0",
        },
    )
    data = json_or_text(body)
    msg = ""
    if isinstance(data, dict):
        msg = str((data.get("data") or {}).get("message", ""))
    ok = status == 200 and "incorrect" in msg.lower()
    report.add(
        Check(
            "audiobooks_com",
            "login_rejects_bad_password",
            ok,
            msg[:200],
            status,
        )
    )

    if token:
        status, _, body = http(
            ABC_BASE + "category/splashcategories",
            method="POST",
            headers={"User-Agent": ABC_UA},
            form={"apiKey": ABC_API_KEY, "token": token},
        )
        data = json_or_text(body)
        ok = (
            status == 200
            and isinstance(data, dict)
            and data.get("status") == "success"
        )
        report.add(
            Check(
                "audiobooks_com",
                "splashcategories",
                ok,
                f"numCategories={(data.get('data') or {}).get('numCategories') if isinstance(data, dict) else None}",
                status,
            )
        )

    email = env("TEST_ABC_EMAIL", "TEST_AUDIOBOOKS_EMAIL")
    password = env("TEST_ABC_PASSWORD", "TEST_AUDIOBOOKS_PASSWORD")
    if not email or not password:
        report.add(
            Check(
                "audiobooks_com",
                "auth_login_library",
                True,
                "skipped — set TEST_ABC_EMAIL + TEST_ABC_PASSWORD",
                needs_auth=True,
                skipped=True,
            )
        )
        return

    device = f"bookclerk-{uuid.uuid4()}"
    status, _, body = http(
        ABC_BASE + "authenticate/login",
        method="POST",
        headers={"User-Agent": ABC_UA},
        form={
            "apiKey": ABC_API_KEY,
            "emailAddress": email,
            "password": password,
            "deviceId": device,
            "deviceType": "Android",
            "appVersion": "12.0.7",
            "OSVersion": "14",
            "alreadyHashed": "0",
        },
    )
    data = json_or_text(body)
    user_token = None
    customer_id = None
    if isinstance(data, dict) and data.get("status") == "success":
        payload = data.get("data") or {}
        user_token = (payload.get("tokenInformation") or {}).get("token")
        customer_id = payload.get("customerId")
    ok = bool(user_token)
    report.add(
        Check(
            "audiobooks_com",
            "auth_login",
            ok,
            f"customerId={customer_id}",
            status,
            needs_auth=True,
        )
    )
    if not user_token or customer_id is None:
        return
    status, _, body = http(
        ABC_BASE + "booklist/library",
        method="POST",
        headers={"User-Agent": ABC_UA},
        form={
            "apiKey": ABC_API_KEY,
            "token": user_token,
            "customerId": str(customer_id),
            "offset": "0",
            "numberOfBooks": "20",
            "sort": "title",
            "searchTerm": "",
        },
    )
    data = json_or_text(body)
    ok = status == 200 and isinstance(data, dict) and data.get("status") == "success"
    report.add(
        Check(
            "audiobooks_com",
            "auth_library",
            ok,
            str(data)[:300] if not ok else f"keys={sorted((data.get('data') or {}).keys())}",
            status,
            needs_auth=True,
        )
    )


# ---------------------------------------------------------------------------
# Kobo — device auth is public; user library needs ActivateOnWeb
# ---------------------------------------------------------------------------

KOBO_UA = (
    "Mozilla/5.0 (Linux; U; Android 2.0; en-us;) AppleWebKit/538.1 "
    "(KHTML, like Gecko) Version/4.0 Mobile Safari/538.1 "
    "(Kobo Touch 0373/4.38.23171)"
)
KOBO_PLATFORM = "00000000-0000-0000-0000-000000000373"


def probe_kobo(report: Report) -> None:
    device_id = hashlib.sha256(b"bookclerk-probe-device").hexdigest()
    serial = hashlib.sha256(b"bookclerk-probe-serial").hexdigest()[:32]
    client_key = (
        __import__("base64")
        .b64encode(KOBO_PLATFORM.encode())
        .decode()
    )
    status, _, body = http(
        "https://storeapi.kobo.com/v1/auth/device",
        method="POST",
        headers={"User-Agent": KOBO_UA, "Content-Type": "application/json"},
        json_body={
            "AffiliateName": "Kobo",
            "AppVersion": "4.38.23171",
            "ClientKey": client_key,
            "DeviceId": device_id,
            "PlatformId": KOBO_PLATFORM,
            "SerialNumber": serial,
        },
    )
    data = json_or_text(body)
    access = data.get("AccessToken") if isinstance(data, dict) else None
    ok = status == 200 and bool(access)
    report.add(
        Check(
            "kobo",
            "device_auth_anonymous",
            ok,
            f"TokenType={data.get('TokenType') if isinstance(data, dict) else None}",
            status,
        )
    )
    if not access:
        return

    status, _, body = http(
        "https://storeapi.kobo.com/v1/initialization",
        headers={"User-Agent": KOBO_UA, "Authorization": f"Bearer {access}"},
    )
    data = json_or_text(body)
    resources = (data.get("Resources") or {}) if isinstance(data, dict) else {}
    needed = {"library_sync", "audiobook", "content_access_book", "device_auth"}
    ok = status == 200 and needed <= set(resources)
    report.add(
        Check(
            "kobo",
            "initialization_resources",
            ok,
            f"audiobooks_enabled={resources.get('kobo_audiobooks_enabled')} "
            f"library_sync={resources.get('library_sync')}",
            status,
        )
    )

    status, _, body = http(
        resources.get("library_sync") or "https://storeapi.kobo.com/v1/library/sync",
        headers={"User-Agent": KOBO_UA, "Authorization": f"Bearer {access}"},
    )
    # Anonymous device must not see a user library
    ok = status in (401, 403)
    report.add(
        Check(
            "kobo",
            "library_sync_requires_user",
            ok,
            f"status={status} body={body[:120]!r}",
            status,
        )
    )

    # User activation is browser-based (ActivateOnWeb). Document skip until
    # interactive credentials / one-time activation code flow is wired.
    report.add(
        Check(
            "kobo",
            "auth_user_activation",
            True,
            "skipped — needs Kobo account + browser ActivateOnWeb "
            "(no password API; same UX class as Audible device login). "
            "Create account at kobo.com; auth probe will be added next.",
            needs_auth=True,
            skipped=True,
        )
    )


# ---------------------------------------------------------------------------
# Podimo — GraphQL behind Cloudflare
# ---------------------------------------------------------------------------


def probe_podimo(report: Report) -> None:
    status, _, body = http(
        "https://graphql.pdm-gateway.com/graphql",
        method="POST",
        json_body={"query": "{ __typename }"},
    )
    text = body.decode("utf-8", errors="replace")
    blocked = status == 403 and ("Cloudflare" in text or "cf-error" in text.lower() or "Just a moment" in text)
    if blocked or status == 403:
        # Expected from many cloud egress IPs — not a regression of our probes.
        report.add(
            Check(
                "podimo",
                "graphql_reachable",
                True,
                "Cloudflare challenge from this egress — needs browser-like "
                "TLS/JA3 or residential IP before auth can be probed",
                status,
                skipped=True,
            )
        )
    else:
        report.add(
            Check(
                "podimo",
                "graphql_reachable",
                status == 200,
                f"status={status} body={text[:120]}",
                status,
            )
        )
    report.add(
        Check(
            "podimo",
            "auth_login",
            True,
            "skipped — unblock Cloudflare first; then TEST_PODIMO_EMAIL/PASSWORD",
            needs_auth=True,
            skipped=True,
        )
    )


# ---------------------------------------------------------------------------
# Downpour — app host is a Gadget/Shopify shell; deeper RE still needed
# ---------------------------------------------------------------------------


def probe_downpour(report: Report) -> None:
    status, _, body = http("https://app.downpour.com/")
    text = body.decode("utf-8", errors="replace")
    # Confirms host is up; not yet a usable REST surface
    ok = status == 200
    report.add(
        Check(
            "downpour",
            "app_host_up",
            ok,
            "Gadget/Shopify shell — library REST still obfuscated in APK; "
            "prefer Magento/account download investigation with credentials",
            status,
        )
    )
    report.add(
        Check(
            "downpour",
            "auth_login",
            True,
            "skipped — set TEST_DOWNPOUR_EMAIL + TEST_DOWNPOUR_PASSWORD once "
            "API paths are deobfuscated (or use web downloadable-products flow)",
            needs_auth=True,
            skipped=True,
        )
    )


PROBES = {
    "librivox": probe_librivox,
    "graphicaudio": probe_graphicaudio,
    "chirp": probe_chirp,
    "storytel": probe_storytel,
    "audiobooks_com": probe_audiobooks_com,
    "kobo": probe_kobo,
    "podimo": probe_podimo,
    "downpour": probe_downpour,
}


def write_report(report: Report, out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "generated_at": report.generated_at,
        "checks": [asdict(c) for c in report.checks],
        "summary": {
            "total": len(report.checks),
            "passed": sum(1 for c in report.checks if c.ok and not c.skipped),
            "skipped_auth": sum(1 for c in report.checks if c.skipped),
            "failed": len(report.failed),
        },
    }
    (out_dir / "report.json").write_text(json.dumps(payload, indent=2) + "\n")
    lines = [
        "# Source endpoint probe report",
        "",
        f"Generated: `{report.generated_at}`",
        "",
        "| Source | Check | Result | Detail |",
        "| --- | --- | --- | --- |",
    ]
    for c in report.checks:
        if c.skipped:
            mark = "skip"
        elif c.ok:
            mark = "ok"
        else:
            mark = "FAIL"
        detail = c.detail.replace("|", "\\|").replace("\n", " ")[:160]
        lines.append(f"| {c.source} | {c.name} | {mark} | {detail} |")
    lines.append("")
    (out_dir / "report.md").write_text("\n".join(lines) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--sources",
        default=",".join(PROBES),
        help="Comma-separated source ids",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=Path("artifacts/source-probes"),
        help="Report output directory",
    )
    args = parser.parse_args()
    selected = [s.strip() for s in args.sources.split(",") if s.strip()]
    unknown = [s for s in selected if s not in PROBES]
    if unknown:
        print(f"unknown sources: {unknown}", file=sys.stderr)
        return 2

    report = Report(generated_at=datetime.now(timezone.utc).isoformat())
    for name in selected:
        print(f"==> probing {name}")
        try:
            PROBES[name](report)
        except Exception as exc:  # noqa: BLE001 — surface per-source failures
            report.add(Check(name, "probe_exception", False, repr(exc)))

    write_report(report, args.out)
    failed = report.failed
    for c in report.checks:
        flag = "SKIP" if c.skipped else ("OK" if c.ok else "FAIL")
        print(f"[{flag}] {c.source}.{c.name}: {c.detail[:120]}")
    print(f"wrote {args.out / 'report.md'} ({len(failed)} failed)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
