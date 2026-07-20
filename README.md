# Libation (Rust)

Headless-first Audible library manager — a greenfield Rust rewrite of
[Libation](https://github.com/rmcrackan/Libation), built on
[audible-rs](https://github.com/mkb79/audible-rs).

## Status

**Phase 1 (headless)** covers the full Classic/Chardonnay Liberate + LibationCli
surface: Adrm and Widevine/CENC, optional mp3 re-encode, xHE-AAC preference,
Chardonnay naming templates, classic Libation Files migrate, CLI, `libationd`,
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
| Liberate Adrm aaxc → native decrypt → store | done |
| Liberate Widevine/CENC (CDM `.wvd`, 000307 fallback) | done |
| Prefer xHE-AAC on Widevine path | done |
| `format=mp3` via native Symphonia+LAME re-encode | done |
| Naming templates (`folder_template` / `file_template`) | done (Chardonnay engine) |
| `auth set-scan` / `auth list --bare` (scan inclusion) | done |
| `config template tags` / `config template preview` | done |
| PDF / cover / cue / chapter JSON sidecars + metadata fix-up | done |
| Match existing storage media (`set-status` / `--match-storage`) | done |
| Local FS + S3/MinIO storage (incl. S3 timestamp metadata) | done |
| Classic Libation EF Postgres via `copydb` | done |
| Podcast parent skip + `SavePodcastsToParentFolder` | done |
| `libationd` scheduled scan + auto-liberate + HTTP control plane | done |
| Tantivy library search (`library search`) | done |
| Library export CSV/JSON/XLSX (`library export`) | done |
| PDF-only liberate (`library liberate --pdf`) | done |
| User metadata (tags, ratings, pdf_status) in DB | done |
| Full Chardonnay CLI + settings parity | done — see PR1_PARITY.md |
| GUI | post-PR1 |

## Quick start

```bash
cargo build --workspace

export LIBATION_FILES_DIR=./LibationFiles
cargo run -p libation-cli -- auth login -m us
cargo run -p libation-cli -- library scan
cargo run -p libation-cli -- library liberate --asin B0EXAMPLE
```

### Auth login (OTP / 2FA)

`auth login` drives audible-rs OAuth in a browser (local callback server + QR by
default, or `--external` paste). Amazon accounts with **2FA/MFA must complete
OTP** (authenticator app) or another challenge in that browser session — there
are no CLI password flags. Without interactive access, import an existing
audible-rs `.auth` file (`auth import`) or migrate from classic Libation.

OAuth tokens are stored under `Accounts/<account>.auth`. Prefer encryption at
rest (audible-rs Argon2id + XChaCha20-Poly1305):

```bash
# Explicit passphrase:
export LIBATION_AUTH_PASSWORD='your-strong-passphrase'

# Or point at a secrets path (created with a strong random secret if missing —
# keep this off the Accounts/ volume):
export LIBATION_AUTH_PASSWORD_FILE=/run/libation/secrets/auth_password

libation auth login --force
```

For local throwaway setups only, `auth.allow_plaintext = true` stores unprotected
token files.

### Tools

| Tool | Needed for |
| --- | --- |
| *(none for decrypt/encode)* | Adrm aaxc and Audible Widevine **DASH fMP4/CENC**, MP3, metadata, and chapter split are **native Rust** |
| Android auth + L3 CDM | Widevine / xHE-AAC (`download.widevine=true`) — L3 CDM auto-provisions via classic Libation AudibleCdm; optional BYO `.wvd` |

Audible’s Widevine downloads are a DASH MPD pointing at one CENC **fragmented MP4**
(`moof`/`senc`), offered as AAC-LC and optionally xHE-AAC. We decrypt that path
natively. Progressive (non-DASH) `enca` decrypt is also implemented as a general
CENC fallback, but it is **not** what Audible’s liberate download produces today.

Widevine **L3** (software) is what we support for stereo / xHE-AAC. Spatial/Atmos needs **L1** (hardware) and is not available on desktop — same as classic Libation.

For Widevine titles, use a normal login (registers as Android):

```bash
libation auth login --force
```

On first Widevine liberate, an L3 `.wvd` is fetched from the AudibleCdm provider and cached under `Accounts/<account>.wvd` (override with `download.widevine_cdm`, or set `download.widevine_cdm_provider = "off"` to require BYO only).

No external `ffmpeg` or `aaxclean-cli` binaries are required. When
`download.strip_audible_brand_audio = true`, liberate also trims Audible
pre/post-roll using `brand_intro_duration_ms` / `brand_outro_duration_ms` from
chapter metadata (classic Libation behavior).
### Widevine / xHE / mp3

```toml
[download]
widevine = true
xhe_aac = true
format = "mp3"          # native LAME re-encode after decrypt
# widevine_cdm = "device.wvd"   # optional BYO; otherwise auto-provision L3
folder_template = "<author>/<title>"
file_template = "<title> [<asin>]"
```

Adrm is tried first when `widevine = false`. If Audible returns `000307` (no
aaxc asset), liberate automatically falls back to Widevine when a CDM is found.

### Sidecars and metadata fix-up

```toml
[download]
download_cover = true       # save .jpg alongside audio (DownloadCoverArt)
download_pdf = true         # companion PDF when available
create_cue = true           # .cue from API chapters (CreateCueSheet)
fixup_metadata = true       # embed cover/chapters/tags (AllowLibationFixup)
save_chapter_json = true    # chapters.<tree|flat>.json
cover_size = "500"          # 500 | 1215 | native
chapter_layout = "tree"     # tree | flat
```

Artifact failures are logged and do not fail the audio liberate (classic behavior).

Relative `storage.local.root` values resolve under `LIBATION_FILES_DIR`.

## Fresh install with existing audiobooks

```bash
libation library scan --match-storage
libation library liberate          # skips matched titles
libation library liberate --force  # re-download anyway
```

## Migrate from classic Libation

```bash
export LIBATION_FILES_DIR=./LibationFiles
cargo run -p libation-cli -- migrate import --from ~/Libation --force
```

Imports Settings (including `UseWidevine`, `DecryptToLossy`, `FolderTemplate` /
`FileTemplate`, `AllowLibationFixup`, `DownloadCoverArt`, `CreateCueSheet`), accounts/auth, and `LibationContext.db`.

## License

GPL-3.0-or-later (aligned with upstream Libation). `audible-rs` remains MIT.
