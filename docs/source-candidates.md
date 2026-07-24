# Audiobook source candidates (beyond Audible / Libro.fm)

Research notes on stores that could plug into `libation-source`'s
`ContentSource` trait. Criteria:

1. Public API **or** reverse-engineerable Android APK (Libro.fm pattern)
2. DRM-free audio **or** community-documented decrypt path
3. No DRM mention → treat as **unknown** until investigated

APKs were downloaded via `apkeep` (APKPure) and decompiled with `jadx`
(2026-07-22). Package IDs and key endpoints below are from those APKs plus
community tooling ([audiobook-dl](https://github.com/jo1gi/audiobook-dl),
[kobodl](https://github.com/subdavis/kobo-book-downloader), etc.).

## Fit with current architecture

| Concern | Notes |
| --- | --- |
| Trait surface | `login` / `list_accounts` / `scan` / `fetch_title` → `SourceFetch::Plain` or `Encrypted` |
| Plain path | Matches Libro.fm today (no `libation-decrypt`) |
| Encrypted path | Today: Adrm + Widevine only; new DRM kinds need decrypt work |
| Auth files | Per-account tokens under `Accounts/` |
| Config plugins | `[sources.<id>]` enable + source knobs; `[integrations.*]` reserved for third-party hooks |
| Diagnostics | Top-level `[diagnostics]` only (not an integration plugin) |

Sources are registered from config like a small plugin table: set
`[sources.chirp] enabled = false` (or `LIBATION_SOURCE_CHIRP_ENABLED=0`) to
keep the binary from loading that store. Per-source ingest quality overrides
live on the same table (`ingest = "highest"`), winning over legacy
`[download.ingest.sources]`. Crash/error-burst reporting stays under
`[diagnostics]` — distinct from `[integrations.*]`.

---

## Priority ranking

| Rank | Source | Access | DRM | Integration fit |
| ---: | --- | --- | --- | --- |
| 1 | **GraphicAudio** | Clear Retrofit API in APK | DRM-free (official ZIP / app Lo+Hi URLs) | Excellent — Libro-like |
| 2 | **Kobo Audiobooks** | Documented `storeapi.kobo.com` (kobodl) | Audiobook spine usually plain; ebooks use KDRM | Strong — Audible-like auth |
| 3 | **Chirp** | GraphQL web + Android Mockingjay APIs | URL AES on web; local download encryptor in app; files play as MP3 | Strong with caveats |
| 4 | **Storytel / Mofibo** | Android API (`api.storytel.net`) | Serves `audio/mpeg` after auth (community) | Strong; rate-limit risk |
| 5 | **Audiobooks.com** | `api.audiobooks.com/api/v2/` in APK | Stream page yields MP3 (community) | Good; Storytel-owned |
| 6 | **Downpour** | `app.downpour.com` + web downloads | Mixed: mostly DRM-free, some DRM titles | Medium — filter `isDrm` |
| 7 | **LibriVox** | Public catalog API | Public domain / free | Easy but niche |
| 8 | **Podimo** | GraphQL gateway | Unknown (CDN mp3/m3u8 present) | Medium / investigate |
| — | Google Play Books | Partial official export | Per-title export flag | Weak for library sync |
| — | Spotify / Everand | — | Heavy / app-bound DRM | Poor fit |

---

## 1. GraphicAudio — best next source

**Why:** Same integration shape as Libro.fm: password login → library list →
plain download URLs. Official store also sells DRM-free MP3/M4B/FLAC ZIPs.

**APK:** `com.efsharp.graphicaudio`

**Base URL:** `https://www.graphicaudio.net/access/`

| Method | Path | Role |
| --- | --- | --- |
| `POST` | `activation/login` | Form: `username`, `password`, `client_id` |
| `POST` | `activation/thirdparty` | Token activate |
| `POST` | `activation/remove` | Forget device |
| `GET` | `api/products` | Library (`Authorization` header) |
| `GET` | `api/links?product=` | Download URLs |
| `GET`/`PUT` | `api/bookmark` | Progress |

`DownloadInfoResponse` fields: `Lo` / `Hi` → low/high quality stream/download
URLs (m4a in-app). No Widevine/MediaDrm usage in app code beyond ExoPlayer
boilerplate.

**Libation mapping:** New `SourceKind::GraphicAudio`, `SourceFetch::Plain`,
auth file e.g. `Accounts/*.ga.auth`. Optional: also scrape Magento
“My Downloadable Products” for ZIP MP3/M4B/FLAC purchases.

**Risks:** Niche catalog (dramatized productions). Device activation may
cap concurrent clients (same pattern as many mobile apps).

---

## 2. Kobo Audiobooks

**Why:** Mature community client already emulates device auth and downloads
audiobook spines as numbered audio files. Closest “Audible-sized” commercial
catalog among DRM-tractable options.

**APK:** `com.kobobooks.android` (large; strings confirm audiobook player +
ExoPlayer DRM stack for *ebooks* / streaming)

**API (from kobodl, still current pattern):**

- Device auth: `https://storeapi.kobo.com/v1/auth/device`
- Refresh: `…/v1/auth/refresh`
- Init resources: `…/v1/initialization`
- Library sync: tokenized `library_sync`
- Audiobook download URL → JSON `Spine[]` with `Url` + `FileExtension`

**DRM:** Ebook path uses KDRM / AdobeDrm. Audiobook download path in kobodl
skips content-key decrypt when `DrmType` is not KDRM/AdobeDrm and writes
plain part files. Treat as **plain for typical audiobooks**; verify live
before committing.

**Libation mapping:** Device-activation login (browser code, like Audible
QR/OAuth UX), `SourceFetch::Plain` for spine parts (ZIP/M4B packaging in
liberate). Do **not** need KDRM remover unless we also want ebooks.

**Risks:** Activation UX; API churn; regional audiobook availability.

---

## 3. Chirp (Pubmark)

**Why:** Deal-focused owned library; strong community downloaders; APK exposes
dedicated API hosts.

**APK:** `com.chirpbooks.chirp`

| Constant | Value |
| --- | --- |
| `MOCKINGJAY_API_ROOT` | `https://api.chirpbooks.com/api` |
| `MOCKINGJAY_LISTEN_API_ROOT` | `https://listen-api.chirpbooks.com/api` |
| Web | `https://www.chirpbooks.com` |

Android uses Apollo GraphQL (`com.mockingjay.*` queries/mutations:
`AndroidCurrentUserAudiobooksQuery`, sign-in, archive, position, …) plus
“Kingfisher” playback/download. Web/community path
([audiobook-dl](https://github.com/jo1gi/audiobook-dl)):

1. Cookie session
2. GraphQL `fetchAudiobookTracks` / `fetchAudiobookTrackUrl`
3. AES-CBC decrypt of `webPlayerMediaUrl` (key from page `data-dk`, IV from
   padded `user_id`)
4. Resulting URL is playable **MP3**

App also has `KingfisherDownloadedTrackEncryptor` / ExoPlayer offline cache —
local-at-rest encryption for offline listening, separate from the web URL
obfuscation.

**Libation mapping:** Prefer GraphQL **password** `signIn` (proven live) →
token → library/track queries → decrypt `webPlayerMediaUrl` → `Plain` MP3.
Avoid cookie sessions for headless use. APK Mockingjay schema is a second
source of truth (`scripts/source-probes/`).

**Risks:** Cloudflare (needs a real User-Agent from some egress); URL crypto
may change; Findaway distribution roots appear in cover CDN paths (monitor
for future DRM shifts).

---

## 4. Storytel / Mofibo

**Why:** Large subscription catalog; community client already implements
login + MP3 asset fetch.

**APK:** `grit.storytel.app` (decompiled; Kotlin/`com.storytel.*`)

**Documented API (audiobook-dl):**

| Call | Role |
| --- | --- |
| `POST …/api/login.action` | Password AES-CBC with fixed key/IV, Android UA |
| `GET api.storytel.net/book-details/consumables/{id}` | Metadata |
| `GET api.storytel.net/assets/v2/consumables/{id}/abook` | 302 → MP3 |
| `GET api.storytel.net/playback-metadata/…` | Chapters |
| `POST api.storytel.net/libraries/bookshelf` | Library |

**DRM:** Download responses expected as `audio/mpeg`. Not classic Widevine
file DRM in the community path. Treat liberate as `Plain`, but watch for
CDN/format changes. Aggressive download rate-limits can invalidate sessions.

**Libation mapping:** Password login + JWT; scan bookshelf; `Plain` fetch.
Subscription semantics differ from purchase libraries (titles can leave the
catalog).

**Risks:** Cloudflare; rate limits; ToS / account flags; regional catalogs.

---

## 5. Audiobooks.com (Storytel USA)

**APK:** `com.audiobooks.androidapp`

**API base in APK:** `https://api.audiobooks.com/api/v2/`

**App REST (preferred over cookies):** form posts with APK `apiKey`
(`NetworkConstants.API_KEY`). Live probe: `authenticate/startup` → guest
token; `authenticate/login` (`emailAddress`, `password`, `deviceId`, …);
`booklist/library`; media via `book/mediaurls/{id}`. Cookie scrape of
`/book/stream/{id}` (`ci_session`) is fallback only.

**DRM:** Unknown officially; community obtains plain MP3 streams. Confirm
`book/mediaurls` with a test account before locking the design.

**Fit:** Good if overlapping Storytel work; US catalog / credits model.

---

## 6. Downpour (Blackstone)

**APK:** `com.blackstonehybrid`

**Hosts:** `https://app.downpour.com/` (app API), `cdn.blackstonepublishing.com`,
storefront `downpour.com` (Shopify + UCP commerce — not library download).

Entitlement model includes `isDrm()`. FAQ: most titles DRM-free MP3; some
publisher-required DRM playable only in-app. App uses Readium audiobook
manifests.

**Libation mapping:** Prefer DRM-free web account downloads when available;
app API for library sync, skip/error when `isDrm == true`. Heavy Kotlin
obfuscation → slower RE than GraphicAudio.

**Risks:** Mixed DRM; obfuscated client; need live classification of titles.

---

## 7. LibriVox

**Public API:** `https://librivox.org/api/feed/audiobooks` (+ audiotracks,
authors). No auth. DRM-free public domain.

**Fit:** Trivial `Plain` source; valuable for tests / CI without credentials,
not a commercial library manager feature.

---

## 8. Podimo

**APK:** `com.podimo`

**Hosts:** `https://graphql.pdm-gateway.com/graphql`, CDN
`cdn.podimo.com/audios/…mp3|m3u8`.

**DRM:** No clear documentation. Presence of bare MP3 CDN URLs is promising
but unproven for audiobook entitlements. Community support in audiobook-dl
via password login.

**Fit:** Worth a follow-up GraphQL schema dump; not first priority.

---

## Lower priority / poor fit

| Source | Verdict |
| --- | --- |
| **Google Play Books** | Some titles offer official M4A export; no good library/download API for automation; eBook DRM tooling ≠ audiobooks |
| **Spotify Audiobooks** | App-bound DRM; no credible personal-library liberate path |
| **Everand / Scribd** | Streaming subscription; cookie scrapers exist; DRM/ToS hostile to Libation model |
| **Nextory** | audiobook-dl login support; APK not fetched this pass; treat DRM as unknown |
| **OverDrive / Libby / cloudLibrary** | Library lending, not ownership; different product semantics |

---

## Recommended implementation order

1. **GraphicAudio** — smallest Libro-shaped spike (`ContentSource` + Plain
   liberate + APK probe script).
2. **Kobo audiobooks** — reuse kobodl protocol knowledge; device-activation
   auth UX.
3. **Chirp** — GraphQL + URL decrypt; optional APK Mockingjay probe.
4. **Storytel / Audiobooks.com** — shared Storytel-family learnings; handle
   subscription semantics explicitly in UX/docs.
5. **Downpour** — only after `isDrm` filtering is validated live.

LibriVox can land anytime as a zero-auth sandbox source.

## Live endpoint probe (2026-07-23)

API-first probes (no cookies) live in `scripts/source-probes/`. Re-run:

```bash
python3 scripts/source-probes/probe_all.py
```

| Source | Unauth result | Notes |
| --- | --- | --- |
| **GraphicAudio** | OK | Public samples + plain `audio/mp3` (`ID3`); login returns 401 JSON for bad password |
| **Chirp** | OK | GraphQL alive; `signIn` returns “Invalid username or password” (password API) |
| **Storytel** | OK | `INVALID_CREDENTIALS`; bookshelf 401 without JWT |
| **Audiobooks.com** | OK | Guest `authenticate/startup` token; login rejects bad password; splashcategories OK |
| **Kobo** | OK | Anonymous device auth + init (`kobo_audiobooks_enabled`); library_sync 401 without user |
| **LibriVox** | OK | Public JSON catalog (needs a real `User-Agent` — CF 1010 otherwise) |
| **Downpour** | Partial | `app.downpour.com` up (Gadget shell); library REST still obfuscated |
| **Podimo** | Blocked here | GraphQL behind Cloudflare from cloud egress |

Preference confirmed: **mobile/API auth over stored cookies** for GA, Chirp,
Storytel, Audiobooks.com, and Kobo device auth.

## Live auth smoke (2026-07-23)

Test accounts (empty owned libraries except Chirp auto-entitlement):

| Source | Result |
| --- | --- |
| GraphicAudio | Login OK; scan upserts **0** owned titles (5 promotional samples skipped) |
| Chirp | Login OK (`user_id=5330622`); scan found **Firefly: Big Damn Hero** (`product_id=444622`) |

Configure passwords via env (never argv):

```bash
export LIBATION_GA_PASSWORD='…'
export LIBATION_CHIRP_PASSWORD='…'
libation auth login --source graphicaudio --email you+ga@example.com
libation auth login --source chirp --email you+chirp@example.com
libation library scan --source graphicaudio
libation library scan --source chirp
# After verification, disable scan inclusion:
libation auth set-scan <account> --scan false
```

Probe harness still uses `TEST_GA_*` / `TEST_CHIRP_*` for Python smoke tests.

### Credentials for remaining sources

| Source | Env vars | Notes |
| --- | --- | --- |
| Storytel | `TEST_STORYTEL_EMAIL`, `TEST_STORYTEL_PASSWORD` | + `pycryptodome`; CLI later: `LIBATION_STORYTEL_PASSWORD` |
| Audiobooks.com | `TEST_ABC_EMAIL`, `TEST_ABC_PASSWORD` | APK `apiKey` already in probe |
| Kobo | *(browser ActivateOnWeb)* | No password API |
| Downpour | `TEST_DOWNPOUR_EMAIL`, `TEST_DOWNPOUR_PASSWORD` | After REST deobfuscation |
| Podimo | `TEST_PODIMO_EMAIL`, `TEST_PODIMO_PASSWORD` | After Cloudflare |
| LibriVox | — | None |

Keep `library.auto_liberate = false`, disable scan after login, liberate ≤1 title.

## GraphicAudio purchase tiers → Libation access paths

Store SKUs (example: *Red Rising 1 of 2*, base **$12**):

| Cart option | Price delta | Magento ZIP | Browser Player (`/library`) | Access App API (`/access`) |
| --- | ---: | --- | --- | --- |
| Listen with Access App and Browser Player | +$0 | No | Yes | Yes (≤4 devices) |
| MP3 Zip Download + App + Browser | +$2 | Yes (≤3 attempts) | Yes | Yes |
| M4B Zip Download + App + Browser | +$2 | Yes (≤3 attempts) | Yes | Yes |
| FLAC Zip Download + App + Browser | +$3 | Yes (≤3 attempts) | Yes | Yes |

Official docs: ZIP downloads also unlock App + Browser; App Access–only does
**not** include a computer ZIP. The **4-device limit** is for Access App
activations (`client_id`); Browser Player is a separate website feature (no
device-slot language in FAQ). Magento ZIP links live under
[My Downloadable Products](https://www.graphicaudio.net/downloadable/customer/products/).

### Expected Libation functionality (`[sources.graphicaudio] access`)

1. **Browser Player (default: `access = "web"`)** — **implemented**  
   Magento login → `library/index/content_library` →
   `/library/player/listen/title/{slug}/` → `<audio src>` on
   `media.graphicaudio.net` → download with **CloudFront signed cookies**
   set by the player page (bare URL without cookies returns 403). No
   device slot. Requires `LIBATION_GA_PASSWORD` (same as login). Login does
   **not** register an Access App device.

2. **Magento ZIP (opt-in: `access = "zip"`)** — **implemented**  
   Magento customer session → My Downloadable Products → follow download
   link (307 → signed CloudFront ZIP) → extract M4B/MP3/FLAC →
   `SourceFetch::Plain` (`m4b_path` when M4B). No Access App device slot.
   Each Magento link hit consumes one of ≤3 download attempts — transfer
   must complete after resolve. Same password env as Browser Player.

3. **Access App API (opt-in: `access = "device"`)** — **implemented** (streaming)  
   `POST /access/activation/login` + `api/products` + `api/links` → Hi/Lo
   M4A streamed to disk (no full-file RAM buffer). Consumes one of four
   device slots per `client_id`. `POST /access/activation/remove` with
   `client_id` drops a slot. Reuse a stable `client_id` from `.ga.auth`.
   Only this mode registers a device at `auth login`.

**Config:**

```toml
[sources.graphicaudio]
enabled = true
access = "web"   # web | zip | device
# ingest = "highest"   # optional override of [download.ingest]
```

Env override: `LIBATION_GA_ACCESS` (legacy alias `LIBATION_GA_FETCH`).
Disable registration with `enabled = false` or
`LIBATION_SOURCE_GRAPHICAUDIO_ENABLED=0`. There is no ZIP→Browser→App cascade —
pick one path explicitly (default web).

### Live purchase probe (Sons of Ares Vol. 1, product `5273`)

- SKU: **M4B Zip Download + App + Browser** (order 1001007764)
- Magento ZIP: `media.graphicaudio.net/m4b-256kbps/SONSOFARES01-m4b.zip`
  (~464 MB) → single `SONSOFARES01.m4b` (~521 MB, Nero `chpl` chapters)
- Browser / App Hi: same ~521 MB M4A object under `app-high/`
- App Lo: `app-normal/…_lo.m4a` (~198 MB, signed query string)
- Remaining Magento downloads after probe: **2** (one attempt used to
  capture the CDN redirect; Browser Player liberate does not consume ZIP
  attempts)
- After `activation/remove`, re-run `auth login --force` (same `client_id`)
  before scan — stale tokens may list samples only until refreshed

### What to purchase for testing

**Buy one title as M4B Zip Download** (≈ base + $2), not App Access–only.

That single SKU unlocks **ZIP + Browser + App**, so we can validate all three
paths without a second purchase. Prefer a short/cheap title. After purchase:

1. Confirm ZIP under My Downloadable Products (primary liberate path).
2. Probe Browser Player network calls while logged into Magento (session OK for
   discovery; implement API-first afterward if possible).
3. Only then exercise Access App login if (1)/(2) miss media — and remove the
   Libation `client_id` when done.

App Access–only first is only worthwhile for the cheapest Browser Player RE
experiment if you accept buying ZIP later.

**Note:** Earlier cloud probe logins used a Libation `client_id` against the
test GA account and may have registered a device — check Account → Access App
device management and remove unknown devices.

## Open questions before coding

- Confirm owned-title `api/links` signed-URL TTL vs Browser Player CloudFront
  cookie TTL in long liberates.
- Live-check one Kobo audiobook spine for empty/`None` DRM fields.
- Decide whether subscription sources (Storytel, Podimo) belong in Libation
  (owned-library product) or stay out of scope.
- Chirp: **prefer GraphQL password `signIn`** over cookie sessions for
  headless/daemon use (probe confirms this path).
