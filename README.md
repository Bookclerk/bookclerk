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
| Liberate Adrm aaxc → aaxclean → store | done |
| Liberate Widevine/CENC (CDM `.wvd`, 000307 fallback) | done |
| Prefer xHE-AAC on Widevine path | done |
| `format=mp3` via ffmpeg re-encode | done |
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

### Tools

| Tool | Needed for |
| --- | --- |
| [aaxclean-cli](https://github.com/Mbucari/aaxclean-cli) | Adrm decrypt; preferred CENC decrypt (`AUDIBLE_AAXCLEAN_CLI`) |
| `ffmpeg` | CENC decrypt fallback; `format=mp3` re-encode (`LIBATION_FFMPEG`) |
| Widevine `.wvd` CDM | Widevine-only titles / `download.widevine=true` |

Place the CDM at `download.widevine_cdm`, `{LIBATION_FILES_DIR}/widevine.wvd`, or
`Accounts/<account>.wvd`.

Adrm decrypt currently shells out to `aaxclean-cli`. A **native Rust
reimplementation of aaxclean** (inside `libation-decrypt`) is a planned
post-PR1 goal so liberate no longer depends on that external binary.

### Widevine / xHE / mp3

```toml
[download]
widevine = true
xhe_aac = true
format = "mp3"          # requires ffmpeg
widevine_cdm = "device.wvd"
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
fixup_metadata = true       # ffmpeg embed cover/chapters/tags (AllowLibationFixup)
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
