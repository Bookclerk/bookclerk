# Libro.fm APK API probe

The Libro.fm integration in `crates/bookclerk-plugins/optional/source-libro` follows the unofficial
mobile API used by the Android app (`fm.libro.librofm`). Community clients
originally reverse-engineered those calls; this probe keeps that surface
honest by re-extracting it from the latest Play Store APK.

## API version (`/api/vN/`) is not locked

Client path constants currently show whatever major version the latest APK
uses (e.g. `/api/v12/…`). That value is **extracted**, not hard-coded into the
probe:

1. jadx finds `AppModule` / `BuildConfig` → `/api/vN/`
2. Drift vs `LIBRARY_PATH` / `DOWNLOAD_MANIFEST_PATH` / `PACKAGED_M4B_PATH`
3. Live smoke derives explore URLs from the library path’s `/api/vN/` prefix
4. The auto-sync PR rewrites the Rust constants when the major bumps

A fallback that assumed `v12` would hide the very change this workflow exists
to catch — there is no such fallback.

## What APK analysis can (and cannot) do

### Captured from the decompiled APK (jadx)

- `BuildConfig` / `AppModule` — base URL, `/api/vN/` prefix, app version
- `AuthInterceptor` — `X-LibroFm-*` and `Authorization` headers
- OkHttp embedded user-agent (`okhttp/x.y.z`)
- Retrofit `@GET` / `@POST` / … paths on `*Api.java` interfaces
- **Request contract**: `@Query` / `@Path` / `@Body` parameter names
- **Response / body shapes**: Gson `@SerializedName` + nested DTO fields
- OAuth password-grant path (`/oauth/token`) plus auth request/response fields

Then it diffs tracked path/header/version constants against
`crates/bookclerk-plugins/optional/source-libro/src/client.rs` and request/response shapes against
`scripts/librofm-apk-probe/expected_shapes.json`.

| Client constant | APK source |
| --- | --- |
| `DEFAULT_BASE_URL` | `AppModule.provideApiBaseUrl` / `BuildConfig.BASE_URL` |
| `OAUTH_TOKEN_PATH` | `LoginRepoImpl` → `{base}/oauth/token` |
| `LIBRARY_PATH` | `{api_prefix}library` |
| `DOWNLOAD_MANIFEST_PATH` | `{api_prefix}download-manifest` |
| `PACKAGED_M4B_PATH` | `{api_prefix}audiobooks/{isbn}/packaged_m4b` |
| `APP_VER` | APK `versionName` / `BuildConfig.VERSION_NAME` |
| `USER_AGENT_VALUE` | OkHttp `userAgent` |

### Limits of static APK analysis

Declared Gson/Retrofit contracts are **not** a live wire dump. We auto-update
path/version/UA constants after a successful **auth** smoke. We do **not**
auto-rewrite Rust `serde` structs.

## Live API validation (auth required for acquire path)

`bookclerk-libro` uses oauth → library → `download-manifest?format=m4b` →
packaged_m4b / ZIP download-manifest → CDN bytes. Those calls need a library
account. Catalog `explore/*` exists and is unauthenticated, but the client does
not use it today — public smoke is informational only.

### Auth smoke (CI gate)

Requires repository secrets:

- `TEST_LIBRO_EMAIL` or `TEST_LIBRO_USERNAME` or `TEST_LIBRO_USER`
- `TEST_LIBRO_PASSWORD`
- optional `TEST_LIBRO_CATALOG_ISBN` — any catalog ISBN for auth metadata
  (**does not need to be in the library**; default `9780307749703`)
- optional `TEST_LIBRO_ISBN` — owned library ISBN for download/media probe
- optional `TEST_LIBRO_REQUIRE_MEDIA` — fail if the account owns no titles
- optional `TEST_LIBRO_MAX_DOWNLOAD_BYTES` — default `104857600` (100 MiB)

**Empty-library dedicated accounts work** for oauth + library list + authenticated
catalog metadata. Media/CDN probing still needs ownership of at least one title
(or set `TEST_LIBRO_REQUIRE_MEDIA` and keep one short book in the account).

Flow:

1. OAuth password grant
2. Library page 1 (+ schema check; empty OK)
3. Authenticated `explore/audiobook_details/{catalog_isbn}` — works when
   `purchase_info.owned=false`
4. If an owned ISBN exists: `download-manifest?format=m4b` + packaged_m4b +
   ZIP download-manifest + download **one** media asset and probe magic bytes
   (`ftyp` / MP3 / zip+audio)
5. If no owned ISBN: skip download/media (pass) unless `TEST_LIBRO_REQUIRE_MEDIA`

Verified live:

| Endpoint (auth) | Owned ISBN | Unowned catalog ISBN |
| --- | --- | --- |
| `explore/audiobook_details/{isbn}` | 200 | 200 (`owned=false`) |
| `download-manifest?format=m4b` | 200 (`.m4b` part) | 404 |
| `download-manifest` (ZIP) | 200 | 404 |
| `packaged_m4b` | 200 / 404 | 404 |
| `users/metadata/by_isbn` | 200 | 404 |

Constant auto-PRs run only when auth smoke **passes** after APK API drift.
If secrets are missing on drift, CI opens an issue instead of a PR.

### When live smoke runs (CI)

Weekly APK extract always runs. **Live** oauth / library / download / media
checks run only when the probe reports API drift:

- any `severity=error` drift (paths, query params, version, …), or
- any `schema.*` drift vs `expected_shapes.json`

Informational-only extras (e.g. unused `X-LibroFm-Api-Key` in the APK) do **not**
trigger live calls. Override with `workflow_dispatch` → `force_live_smoke`.

### Public catalog (optional)

`live_smoke.py --profiles public` hits `explore/search`, suggest, genres, and
`explore/audiobook_details/{isbn}` without auth. Useful for catalog metadata
shapes; not a substitute for acquire-path checks.

## Local usage

```bash
# Download latest APK + decompile + diff (needs network + Java for jadx)
python3 scripts/librofm-apk-probe/extract_libro_api.py

# Auth smoke with media download + probe (needs credentials)
export TEST_LIBRO_EMAIL='you@example.com'
export TEST_LIBRO_PASSWORD='…'   # never on argv
# optional empty-library account:
#   export TEST_LIBRO_CATALOG_ISBN='9780307749703'  # unowned metadata OK
# optional owned title for CDN media:
#   export TEST_LIBRO_ISBN='978…'
python3 scripts/librofm-apk-probe/live_smoke.py --profiles current,apk

# JSON-only (skip CDN bytes)
python3 scripts/librofm-apk-probe/live_smoke.py --profiles current --no-media-download

# Public catalog only
python3 scripts/librofm-apk-probe/live_smoke.py --profiles public
```

Reports land in `artifacts/librofm-apk-probe/` (`report.md`, `report.json`,
`apk_shapes.json`, `live_smoke.json`). Exit `1` = drift or smoke/media failure;
exit `2` = hard failure / missing auth credentials.

## CI

`.github/workflows/librofm-apk-probe.yml` runs weekly and on `workflow_dispatch`.

1. Extract APK API surface; upload **report JSON/Markdown only** (never APKs or
   media). Live smoke uses `--no-media-download`.
2. **Only if APK API drifted** (or `force_live_smoke`): public catalog smoke +
   auth live-smoke (API JSON only — no CDN media artifact)
3. On blocking constant drift (schedule/manual): open a constants PR only if
   auth smoke passed; otherwise open an issue (missing secrets or smoke failure)

Wiremock tests bind to the path constants, so constant bumps do not require
hand-editing fixtures.
