# AGENTS.md

## Cursor Cloud specific instructions

This is a Rust workspace (edition 2021, `rust-toolchain.toml` pins the `stable`
channel with `rustfmt` + `clippy`). The startup update script runs
`cargo fetch`, so dependencies are already downloaded when a session begins.

Product docs live under [`docs/`](docs/README.md). Bookclerk is a
**multi-storefront** audiobook library manager (sources + destinations +
integrations), not an Audible-only Libation fork.

### Services / binaries

Two runnable binaries (the workspace `default-members`):

- `bookclerk-cli` (binary `bookclerk`) — headless library manager CLI
  (Audible, Libro.fm, Chirp, GraphicAudio, plugins).
- `bookclerkd` — long-running daemon with an authenticated HTTP API / GUI.

Frontend sources live in `ui/` (Vite/React); build with `npm ci && npm run build`
so `bookclerkd` can serve `ui/dist`. See `docs/gui.md`. Native desktop/tray is
deferred (do not add Tauri/GTK3-pinned shells while RUSTSEC advisories remain).

### Build / lint / test (mirrors `.github/workflows/ci.yml`)

- Build: `cargo build --workspace`
- Format check: `cargo fmt --all -- --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` (CI treats
  warnings as errors via `RUSTFLAGS="-D warnings"`).
- Test: `cargo test --workspace`
- Release binaries: `cargo build --release -p bookclerk-cli -p bookclerkd`

### Running the apps

Set `BOOKCLERK_FILES_DIR` to a writable dir; on first use the app creates
`library.db` (SQLite, bundled — no external DB needed), plus `cache/`, `logs/`
(reserved; Bookclerk does not rotate log files), `search_index/`, and `plugins/`
under it. Third-party plugins are discovered from `plugin.toml` under
`plugins/` (and `BOOKCLERK_PLUGIN_DIRS`); enablement and knobs live in
`config.toml` (see `docs/plugins.md`).
Logging goes to stderr and, when available, the OS facility (journald /
macOS os_log / Windows Event Log); secrets are always redacted (exact values
from config/env/auth including percent-encoded forms, plus patterns; uploads
abort if a registered secret remains). Opt-in reports: `diagnostics.share_reports
= true` with URL from `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` at `cargo build` (CI
sets from `DIAGNOSTICS_COLLECTOR_BASE_URL`) or `collector_url` in config. The
diagnostics ring always keeps TRACE+; stderr/OS facility honor `BOOKCLERK_LOG` /
`RUST_LOG`. Each upload gets a Worker `report_id` UUID (B2 object name). See
`docs/diagnostics.md`.

- CLI: `BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerk-cli -- <cmd>`
  (e.g. `version`, `auth list`, `library list`).
- Daemon: `BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerkd`.
  It listens on `127.0.0.1:8787` by default (override with
  `BOOKCLERK_DAEMON_LISTEN` or `daemon.listen` in `config.toml`). Operator auth
  defaults on (`operator.token` under the files dir, or
  `BOOKCLERK_OPERATOR_TOKEN`). Control plane: `GET /health`,
  `POST /api/auth/login`, authenticated `/api/status`, `/api/jobs`,
  `/api/library/*` (legacy `/status` `/scan` `/acquire` `/jobs` also gated).
  `POST` bodies require the `Content-Type: application/json` header (send `{}`
  for defaults), otherwise the request is rejected.

### Live store / storage testing constraints

When exercising real store credentials in this cloud environment:

- Prefer **interactive** `bookclerk auth login` (browser/QR or Desktop pane), not
  a pre-baked `.auth` file, when the goal is to test Audible login itself.
- Amazon accounts with **2FA/MFA require OTP** during the browser OAuth step
  (audible-rs has no password CLI). Use a TOTP seed or complete the challenge
  in the Desktop pane; see README / `crates/bookclerk-audible/README.md`.
- Password stores (never put passwords on argv):
  - Libro.fm: `auth login --source libro --email <addr>` + `BOOKCLERK_LIBRO_PASSWORD`
  - Chirp: `auth login --source chirp --email <addr>` + `BOOKCLERK_CHIRP_PASSWORD`
  - GraphicAudio: `auth login --source graphicaudio --email <addr>` + `BOOKCLERK_GA_PASSWORD`
- Keep `library.auto_acquire = false` in `$BOOKCLERK_FILES_DIR/config.toml`.
- After login, **disable the account for scans**:
  `bookclerk auth set-scan <account> --scan false`.
  (Scan inclusion is per-account in SQLite, not a TOML key.)
- Do **not** acquire the full library. Cap at **one** book:
  - Audible: `bookclerk library acquire --asin <ASIN>`
  - Libro.fm / others: `bookclerk library acquire --isbn <ISBN>` (or UUID / product id)
- Drive verification with the **CLI**, not `bookclerkd` job triggers (`POST
  /scan` / `/acquire`), so nothing can bulk-queue work.
- One-shot library sync without flipping scan back on: pass an explicit
  account (`bookclerk library scan --account <id>`). Explicit account targets
  bypass `scan_enabled`; bare `library scan` / daemon scheduled scans honor
  it and will skip disabled accounts. Optional
  `--source audible|libro|chirp|graphicaudio` limits which store is scanned.

### Non-obvious gotchas

- Scanning/acquiring requires real store credentials for the sources in use.
  Without a configured account, `scan`/`acquire` jobs fail with "no accounts
  configured" — expected; the daemon + control plane still run for everything
  else. Tokens live under `Accounts/` (Audible `*.audible.auth`, Libro
  `*.libro.auth`, GraphicAudio `*.ga.auth`, Chirp `*.chirp.auth`). Prefer Audible
  encryption via `BOOKCLERK_AUTH_PASSWORD` or `BOOKCLERK_AUTH_PASSWORD_FILE` /
  `[auth].password_file` (missing password-file paths are auto-created with a
  strong random secret — use a secrets volume, not `Accounts/`).
  `auth.allow_plaintext=true` stores unprotected Audible token files.
- Acquire decrypt/encode is fully native in `bookclerk-decrypt` (Adrm aaxc,
  Widevine DASH/CENC, MP3 via Symphonia+LAME, metadata fix-up, chapter split).
  No `ffmpeg` or `aaxclean-cli` is required. Widevine L3 CDMs auto-provision via
  classic Libation AudibleCdm (`auth login` registers as Android);
  optional BYO `.wvd` still works. Spatial/Atmos (L1) is not available. Neither
  a CDM nor ffmpeg is required to build, test, or run non-acquire commands.
- S3/MinIO credentials prefer `Accounts/*.s3.auth` (default
  `Accounts/default.s3.auth`, or `[output.s3].credentials_file` /
  `BOOKCLERK_OUTPUT_S3_CREDENTIALS_FILE`). `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY` still override when both are set; otherwise the AWS
  SDK default provider chain applies (same as AWS CLI: `~/.aws/credentials`,
  SSO, EC2/ECS/EKS roles — CLI install not required). Bucket/region/endpoint/
  path-style come from `BOOKCLERK_OUTPUT_S3_*` (or familiar `BOOKCLERK_S3_*`)
  env vars or `[output.s3]` in config.toml. Local output uses `[output.local]` /
  `BOOKCLERK_OUTPUT_LOCAL_ROOT`. Multiple destination plugins may be
  `enabled` at once — acquire writes to every enabled destination.
- `BOOKCLERK_S3_ENDPOINT` may be host-only (no scheme); Bookclerk prepends
  `https://` when the value looks like a bare hostname.
