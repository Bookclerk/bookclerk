# AGENTS.md

## Cursor Cloud specific instructions

This is a Rust workspace (edition 2021, `rust-toolchain.toml` pins the `stable`
channel with `rustfmt` + `clippy`). The startup update script runs
`cargo fetch`, so dependencies are already downloaded when a session begins.

### Services / binaries

Two runnable binaries (the workspace `default-members`):

- `libation-cli` (binary `libation`) — headless Audible library manager CLI.
- `libationd` — long-running daemon with an HTTP control plane.

Everything else under `crates/` is a library crate.

### Build / lint / test (mirrors `.github/workflows/ci.yml`)

- Build: `cargo build --workspace`
- Format check: `cargo fmt --all -- --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` (CI treats
  warnings as errors via `RUSTFLAGS="-D warnings"`).
- Test: `cargo test --workspace`
- Release binaries: `cargo build --release -p libation-cli -p libationd`

### Running the apps

Set `LIBATION_FILES_DIR` to a writable dir; on first use the app creates
`library.db` (SQLite, bundled — no external DB needed), plus `cache/`, `logs/`,
and `search_index/` under it.

- CLI: `LIBATION_FILES_DIR=/tmp/LibationFiles cargo run -p libation-cli -- <cmd>`
  (e.g. `version`, `auth list`, `library list`).
- Daemon: `LIBATION_FILES_DIR=/tmp/LibationFiles cargo run -p libationd`.
  It listens on `127.0.0.1:8787` by default (override with
  `LIBATION_DAEMON_LISTEN` or `daemon.listen` in `config.toml`). Control plane:
  `GET /health`, `GET /status`, `POST /scan`, `POST /liberate`, `GET /jobs`.
  `POST` bodies require the `Content-Type: application/json` header (send `{}`
  for defaults), otherwise the request is rejected.

### Live Audible / storage testing constraints

When exercising real Audible credentials in this cloud environment:

- Prefer **interactive** `libation auth login` (browser/QR or Desktop pane), not
  a pre-baked `.auth` file, when the goal is to test login itself.
- Amazon accounts with **2FA/MFA require OTP** during the browser OAuth step
  (audible-rs has no password CLI). Use a TOTP seed or complete the challenge
  in the Desktop pane; see README / `crates/libation-audible/README.md`.
- Keep `library.auto_liberate = false` in `$LIBATION_FILES_DIR/config.toml`.
- After login, **disable the account for scans**:
  `libation auth set-scan <account> --scan false`.
  (Scan inclusion is per-account in SQLite, not a TOML key.)
- Do **not** liberate the full library. Cap at **one** book via an explicit
  ASIN: `libation library liberate --asin <ASIN>`.
- Drive verification with the **CLI**, not `libationd` job triggers (`POST
  /scan` / `/liberate`), so nothing can bulk-queue work.
- One-shot library sync without flipping scan back on: pass an explicit
  account (`libation library scan --account <id>`). Explicit account targets
  bypass `scan_enabled`; bare `library scan` / daemon scheduled scans honor
  it and will skip disabled accounts.

### Non-obvious gotchas

- Actually scanning/liberating a library requires real Audible credentials
  (`libation auth login`). Without a configured account, `scan`/`liberate` jobs
  fail with "no accounts configured" — this is expected, and the daemon +
  control plane still run fine for everything else. OAuth tokens live under
  `Accounts/<account>.auth`. Prefer encryption via `LIBATION_AUTH_PASSWORD` or
  `LIBATION_AUTH_PASSWORD_FILE` / `[auth].password_file` (missing password-file
  paths are auto-created with a strong random secret — use a secrets volume, not
  `Accounts/`). `auth.allow_plaintext=true` stores unprotected token files.
- Liberate decrypt/encode is fully native in `libation-decrypt` (Adrm aaxc,
  Widevine DASH/CENC, MP3 via Symphonia+LAME, metadata fix-up, chapter split).
  No `ffmpeg` or `aaxclean-cli` is required. Widevine L3 CDMs auto-provision via
  classic Libation AudibleCdm (`auth login` registers as Android);
  optional BYO `.wvd` still works. Spatial/Atmos (L1) is not available. Neither
  a CDM nor ffmpeg is required to build, test, or run non-liberate commands.
- S3/MinIO credentials are **env-only** (`AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY`); bucket/endpoint/path-style come from
  `LIBATION_S3_*` env vars or `[storage.s3]` in config.toml.
- `LIBATION_S3_ENDPOINT` may be host-only (no scheme); prepend `https://`
  before use when the value looks like a bare hostname.
