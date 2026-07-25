# Bookclerk

Headless-first multi-source audiobook library manager (CLI + daemon). Bookclerk
grew out of a Rust rewrite of [Libation](https://github.com/rmcrackan/Libation)
and now covers Audible, Libro.fm, Chirp, GraphicAudio, plugins, and a Connect
portal. Audible support is built on
[audible-rs](https://github.com/mkb79/audible-rs).

## Status

**Phase 1 (headless)** covers the Classic/Chardonnay acquire surface (LibationCli
parity): Adrm and Widevine/CENC, optional mp3 re-encode, xHE-AAC preference,
Chardonnay naming templates, classic Libation Files migrate, CLI, `bookclerkd`,
classic EF Postgres `copydb`, S3 timestamps, and podcast handling.

**PR1 verdict:** headless parity with Libation Classic/Chardonnay is complete
aside from GUI and a few intentionally deferred / minor items — see
[docs/PR1_PARITY.md](docs/PR1_PARITY.md).

GUI is deferred post-PR1.

### Phase 1 checklist

| Capability | Status |
| --- | --- |
| Auth login (QR / callback server / external paste) | done |
| Auth list / status / import (`.auth` + AccountsSettings.json) | done |
| Migrate classic Libation Files (Settings / accounts / DB / templates) | done |
| Library scan → SQLite (honors `scan_enabled`) | done |
| Acquire Adrm aaxc → native decrypt → store | done |
| Acquire Widevine/CENC (CDM `.wvd`, 000307 fallback) | done |
| Prefer xHE-AAC on Widevine path | done |
| `format=mp3` via native Symphonia+LAME re-encode | done |
| Naming templates (`folder_template` / `file_template`) | done (Libation NamingTemplate port) |
| Naming profiles (`naming_profile`, default Audiobookshelf) | done |
| `auth set-scan` / `auth list --bare` (scan inclusion) | done |
| `config template tags` / `profiles` / `preview` | done |
| PDF / cover / cue / chapter JSON sidecars + metadata fix-up | done |
| Match existing storage media (`set-status` / `--match-storage`) | done (list audio + metadata probe; optional `--fix-layout`) |
| Local FS + S3/MinIO storage (incl. S3 timestamp metadata) | done |
| Classic Libation EF Postgres via `copydb` | done |
| Podcast parent skip + `SavePodcastsToParentFolder` | done |
| `bookclerkd` scheduled scan + auto-acquire + HTTP control plane | done |
| Tantivy library search (`library search`) | done |
| Library export CSV/JSON/XLSX (`library export`) | done |
| PDF-only acquire (`library acquire --pdf`) | done |
| User metadata (tags, ratings, pdf_status) in DB | done |
| Full Chardonnay CLI + settings parity | done — see PR1_PARITY.md |
| GUI | post-PR1 |

## Quick start

```bash
cargo build --workspace

export BOOKCLERK_FILES_DIR=./BookclerkFiles
cargo run -p bookclerk-cli -- auth login -m us
# Libro.fm (password via env — never on argv):
# export BOOKCLERK_LIBRO_PASSWORD='…'
# cargo run -p bookclerk-cli -- auth login --source libro --email you@example.com
cargo run -p bookclerk-cli -- library scan
cargo run -p bookclerk-cli -- library acquire --asin B0EXAMPLE
# UUID / ISBN also work: --isbn 978… or positional title ids
```

### Multi-source (Audible + Libro.fm)

Library rows are keyed by a stable **UUID**; ASIN and ISBN are indexed attributes.
`library acquire` / search accept UUID, ASIN, ISBN, or source product id.
`library scan` syncs every configured source (or `--source audible|libro`); after
scan, non-Audible rows (e.g. Libro.fm) are best-effort enriched with an Audible
ASIN via the shared `bookclerk-enrich` crate (public catalog search + Audnexus,
AudioBookshelf-style confidence scoring, plus ISBN / narrator / subtitle when
available) when `library.enrich_from_audible` is true (default). Exact ISBN
matches boost confidence but do not auto-accept (multiple ASINs can share an
ISBN). No Audible account is required. Set `library.enrich_min_confidence`
(default `90`) to raise or lower the acceptance threshold, or set enrichment
to false to disable.

### Auth login (OTP / 2FA)

`auth login` (default `--source audible`) drives audible-rs OAuth in a browser
(local callback server + QR by default, or `--external` paste). Amazon accounts
with **2FA/MFA must complete OTP** (authenticator app) or another challenge in
that browser session — there are no Audible CLI password flags. Without
interactive access, import an existing audible-rs `.auth` file (`auth import`)
or migrate from classic Libation.

Libro.fm login: `auth login --source libro --email <addr>`. Password comes from
`BOOKCLERK_LIBRO_PASSWORD` or an interactive prompt (never pass it on argv).

OAuth / token files live under `Accounts/` (Audible `.auth`, Libro `.libro.auth`).
Prefer encryption at rest for Audible (audible-rs Argon2id + XChaCha20-Poly1305):

```bash
# Explicit passphrase:
export BOOKCLERK_AUTH_PASSWORD='your-strong-passphrase'

# Or point at a secrets path (created with a strong random secret if missing —
# keep this off the Accounts/ volume):
export BOOKCLERK_AUTH_PASSWORD_FILE=/run/bookclerk/secrets/auth_password

bookclerk auth login --force
```

For local throwaway setups only, `auth.allow_plaintext = true` stores unprotected
token files.

### Tools

| Tool | Needed for |
| --- | --- |
| *(none for decrypt/encode)* | Adrm aaxc and Audible Widevine **DASH fMP4/CENC**, MP3, metadata, and chapter split are **native Rust** |
| Android auth + L3 CDM | Widevine / xHE-AAC (`output.widevine=true`) — L3 CDM auto-provisions via classic Libation AudibleCdm; optional BYO `.wvd` |

Audible’s Widevine downloads are a DASH MPD pointing at one CENC **fragmented MP4**
(`moof`/`senc`), offered as AAC-LC and optionally xHE-AAC. We decrypt that path
natively. Progressive (non-DASH) `enca` decrypt is also implemented as a general
CENC fallback, but it is **not** what Audible’s acquire download produces today.

Widevine **L3** (software) is what we support for stereo / xHE-AAC. Spatial/Atmos needs **L1** (hardware) and is not available on desktop — same as classic Libation.

For Widevine titles, use a normal login (registers as Android):

```bash
bookclerk auth login --force
```

On first Widevine acquire, an L3 `.wvd` is fetched from the AudibleCdm provider and cached under `Accounts/<account>.wvd` (override with `output.widevine_cdm`, or set `output.widevine_cdm_provider = "off"` to require BYO only).

No external `ffmpeg` or `aaxclean-cli` binaries are required. When
`output.strip_audible_brand_audio = true`, acquire also trims Audible
pre/post-roll using `brand_intro_duration_ms` / `brand_outro_duration_ms` from
chapter metadata (classic Libation behavior).
### Widevine / xHE / mp3

```toml
[output]
widevine = true
xhe_aac = true
format = "single_mp3"   # native LAME re-encode after decrypt
# widevine_cdm = "device.wvd"   # optional BYO; otherwise auto-provision L3
# naming_profile = "audiobookshelf"  # default; or "classic" for Libation desktop defaults
# folder_template / file_template override the profile when set
# audiobookshelf folder ≈
#   <first author>/<has series-><first series>/<-has><has series#-><series#> - <-has><has year-><year> - <-has><title short><has narrator-> {<first narrator>}<-has>
# audiobookshelf file = <title> [<asin>]
# classic folder = <title short> [<id>]
# classic file = <title> [<id>]
folder_template = "<author>/<title>"
file_template = "<title> [<asin>]"
```

Adrm is tried first when `widevine = false`. If Audible returns `000307` (no
aaxc asset), acquire automatically falls back to Widevine when a CDM is found.

### Sidecars and metadata fix-up

```toml
[output]
download_cover = true       # save .jpg alongside audio (DownloadCoverArt)
download_pdf = true         # companion PDF when available
create_cue = true           # .cue from API chapters (CreateCueSheet)
fixup_metadata = true       # embed cover/chapters/tags (AllowLibationFixup)
save_chapter_json = true    # chapters.<tree|flat>.json
cover_size = "500"          # 500 | 1215 | native
chapter_layout = "tree"     # tree | flat
```

Artifact failures are logged and do not fail the audio acquire (classic behavior).

Relative `output.local.root` values resolve under `BOOKCLERK_FILES_DIR`.

## Fresh install with existing audiobooks

```bash
bookclerk library scan --match-storage
# Optional: relocate matched files + sidecars onto the naming-profile layout
# (also library.fix_storage_layout / BOOKCLERK_FIX_STORAGE_LAYOUT; default false)
bookclerk library scan --match-storage --fix-layout
bookclerk library acquire          # skips matched titles
bookclerk library acquire --force  # re-download anyway
```

Matching lists acquired audio (`.m4b` / `.mp3` / `.m4a` / `.flac` / `.aac` /
`.ogg` / `.oga`) and probes custom object metadata
(S3 `HeadObject` user-metadata / local `.bookclerk-meta.json`) without downloading
bodies, then falls back to ASIN/ISBN tokens in the path.

## Migrate from classic Libation

```bash
export BOOKCLERK_FILES_DIR=./BookclerkFiles
cargo run -p bookclerk-cli -- migrate import --from ~/Bookclerk --force
```

Imports Settings (including `UseWidevine`, `DecryptToLossy`, `FolderTemplate` /
`FileTemplate`, `AllowLibationFixup`, `DownloadCoverArt`, `CreateCueSheet`), accounts/auth, and `LibationContext.db`.

## License

GPL-3.0-or-later (aligned with upstream Libation). `audible-rs` remains MIT.
