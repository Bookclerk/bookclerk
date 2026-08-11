# AGENTS.md

## Cursor Cloud specific instructions

This is a Rust workspace (edition 2021, `rust-toolchain.toml` pins the `stable`
channel with `rustfmt` + `clippy`). Cloud Agent / Builds use the shared image in
[`.devcontainer/Dockerfile`](.devcontainer/Dockerfile) via
[`.cursor/environment.json`](.cursor/environment.json); `install` runs
[`.cursor/cloud-agent-install.sh`](.cursor/cloud-agent-install.sh) (`cargo fetch`,
`ui` npm build, `cargo build-app --platform --optional --examples`). A `bookclerkd` terminal runs
`cargo dev --skip-build`. Cloud commits use Cursor’s HSM signing (no custom
signing secrets). Local Dev Containers inherit host `git config` and sign via
the host SSH agent socket (private key never enters the container; see
`docs/devcontainer.md`). Dev Container signing tweaks use project-local
`.git/config` only and are not applied to Cloud Builds.

Environment Builds: keep Builds **off** until the environment’s Builds tab
shows a healthy **SYSTEM/RECURRING** snapshot of `main`. Draft (AGENT/MANUAL)
builds validate config but are never the boot baseline; with Builds on and no
healthy SYSTEM build, new agents resolve `no_healthy_builds` and cold-install.

Workspace-local caches (gitignored; travel with the bind-mounted checkout):

| Path | Role |
| --- | --- |
| `.cargo-home/` | `CARGO_HOME` (registry + git) — set in Dev Container / Cloud Agent / `.envrc` |
| `target/` | `CARGO_TARGET_DIR` (also `[build].target-dir` in `.cargo/config.toml`) |
| `.tmp/` | `TMPDIR` for `cc` / build-script temps (avoids host `/tmp` quota) |
| `BookclerkFiles/` | default `$BOOKCLERK_FILES_DIR` for `cargo dev` |

`cargo reset --yes` wipes `BookclerkFiles/` (not `target/` / `.cargo-home/`).

Product docs live under [`docs/`](docs/README.md). Bookclerk is a
**multi-storefront** audiobook library manager (sources + destinations +
integrations), not an Audible-only Libation fork.

### Services / binaries

Four binaries (the workspace `default-members`):

- `bookclerk-cli` (binary `bookclerk`) — headless library manager CLI
  (Audible, Libro.fm, Chirp, GraphicAudio, plugins).
- `bookclerkd` — long-running daemon with an authenticated HTTP API / GUI.
- `bookclerk-media-worker` — confined child process that runs one codec job.
  Ship it beside the hosts: both look for it there and, with the default
  `media.isolation = "required"`, refuse media work when it is missing rather
  than decode untrusted audio in-process. See `docs/media.md`.
- `bookclerk-jail` — launcher that applies a confinement policy to itself and
  then `exec`s an external plugin guest, so the jail is host-imposed rather than
  requested from the plugin. Also ships beside the hosts; with the default
  `plugins.isolation = "required"` a plugin that cannot be jailed is not loaded.
  Policy travels as JSON in `BOOKCLERK_JAIL_SPEC`. See
  `docs/plugins.md#the-guest-jail`.
- `bookclerk-workerd` — jailed Cloudflare workerd isolate launcher for script
  plugins (ships beside hosts; needs the pinned `workerd` binary).

Optional companion (workspace member, not a default-member):

- `bookclerk-tray` — in-process tray library linked into `bookclerkd` (opens the
  web UI in the browser; Linux `ksni`; Windows/macOS `tray-icon` with GTK
  features off). See `docs/gui.md`.

Frontend sources live in `ui/` (Vite/React); build with `npm ci && npm run build`
so `bookclerkd` can serve `ui/dist`. Do not add Tauri/GTK3-pinned shells while
RUSTSEC advisories remain (tracked in `#44`).

### Build / lint / test (mirrors `.github/workflows/ci.yml`)

Prefer the [Dev Container](docs/devcontainer.md) (`.devcontainer/`) when the host
lacks OpenSSL headers or a matching toolchain — `target/` and `.cargo-home/`
stay on the bind mount so you can run built Linux binaries on the host afterward.

- Build: `cargo build --workspace`
- Format check: `cargo fmt --all -- --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` (CI treats
  warnings as errors via `RUSTFLAGS="-D warnings"`).
- Test: `cargo test --workspace`
- Release binaries: `cargo build --release -p bookclerk-cli -p bookclerkd
  -p bookclerk-media-worker -p bookclerk-jail -p bookclerk-workerd`
  (helpers are not optional — see above)
- Confinement tests no-op without a backend, so demand enforcement when it is
  expected: `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT=1 cargo test --workspace`
  (CI sets this on Linux/macOS for self-confine tests; Windows also sets
  `BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT=1` for AppContainer jail/media).
  A plugin-host test also needs `target/debug/bookclerk-jail`, which
  `cargo test --workspace` builds but `cargo test -p bookclerk-plugin-host` does not.

### External plugins (local dev)

Hosts default to **external guests only** (jailed subprocesses — including
platform `sqlite` / `local`). Product storefronts are optional; reference Echo
examples under `examples/` are CI/dev-only (never packaged).

```bash
cargo reset --yes             # wipe BookclerkFiles/ (stale DB / config)
cargo dev                     # platform: hosts + helpers + sqlite/local, then bookclerkd
cargo dev --optional          # also build/stage optional storefronts
cargo dev --examples          # also stage reference Echo examples
cargo dev --skip-build        # install/stage + exec when target/ is already warm
cargo dev-cli -- version      # same platform build, then CLI
cargo build-app --platform --optional --examples
cargo ensure-workerd          # pinned Cloudflare workerd beside helpers
cargo install-platform --skip-build
cargo stage-plugins --optional --examples --skip-build
cargo test-staged             # handshake against installed + staged guests
```

Optional in-process stores: `cargo build -p bookclerkd --features bundled-plugins`.
See `docs/plugins.md` and `crates/bookclerk-dev/README.md`.

`cargo dev` defaults `BOOKCLERK_FILES_DIR` to `<workspace>/BookclerkFiles`
(override with the env var). On first use the app creates `library.db` (SQLite
by default via the `[database]` plugin — see `docs/database.md`; Cloudflare D1
optional), plus `cache/`, `logs/` (reserved; Bookclerk does not rotate log
files), `search_index/`, and `plugins/` under it. Third-party plugins are discovered from `plugin.toml` under
`plugins/` (and `BOOKCLERK_PLUGIN_DIRS`); enablement and knobs live in
`config.toml` (see `docs/plugins.md`). A guest is confined to its install
directory (read-only), `plugins/<id>/data` (its `HOME`), `plugins/<id>/tmp` (its
`TMPDIR`), and the download cache root. Network consent uses
`[capabilities.network]` domains (approve before enable); redirect hops after an
allowed initial host do not require re-approval.
Logging goes to stderr and, when available, the OS facility (journald /
macOS os_log / Windows Event Log); secrets are always redacted (exact values
from config/env/auth including percent-encoded forms, plus patterns; uploads
abort if a registered secret remains). Opt-in reports: `diagnostics.share_reports
= true` with URL from `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` at `cargo build` (CI
sets from `DIAGNOSTICS_COLLECTOR_BASE_URL`) or `collector_url` in config. The
diagnostics ring always keeps TRACE+; stderr/OS facility honor `BOOKCLERK_LOG` /
`RUST_LOG`. Each upload gets a Worker `report_id` UUID (B2 object name). See
`docs/diagnostics.md`.

- CLI: `cargo run -p bookclerk-cli -- <cmd>` (or `cargo dev-cli -- <cmd>`).
  Examples: `version`, `auth list`, `library list`.
- Daemon: `cargo run -p bookclerkd` (or `cargo dev`). Listens on `127.0.0.1:8787`
  and `[::1]:8787` by default (override with `BOOKCLERK_DAEMON_LISTEN` or
  `daemon.listen` in `config.toml`; string, array, or comma-separated). Operator
  auth defaults on (`operator.token` under the files dir, or
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
  in the Desktop pane; see README / `crates/bookclerk-plugins/optional/source-audible/README.md`.
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
  else. Tokens live in the `encrypted_secrets` DB table (Audible, Libro.fm,
  GraphicAudio, Chirp), sealed with the process DEK from `master.key`
  (XChaCha20-Poly1305, `sealed-v1` format). Set `BOOKCLERK_AUTH_PASSWORD`
  (preferred) or `[auth].password` to wrap `master.key` at rest — strongly
  recommended for production. A later password wraps existing BCK1 via
  `bookclerk config master-key wrap` or daemon config reload.
- Audible Adrm/Widevine decrypt is native inside the Audible plugin (sources
  always return Plain). Host packaging is `bookclerk-media` (MP3 via
  Symphonia+LAME, metadata fix-up, chapter remux). No `ffmpeg` or
  `aaxclean-cli` is required. Widevine L3 CDMs auto-provision via classic
  Libation AudibleCdm (`auth login` registers as Android); optional BYO `.wvd`
  still works. Spatial/Atmos (L1) is not available. Neither a CDM nor ffmpeg is
  required to build, test, or run non-acquire commands.
- S3/MinIO credentials: `BOOKCLERK_AWS_ACCESS_KEY_ID` /
  `BOOKCLERK_AWS_SECRET_ACCESS_KEY` (optional `BOOKCLERK_AWS_SESSION_TOKEN`)
  override when both are set; otherwise `encrypted_secrets` (`kind=s3`,
  `account_id=operator`, `name=default` — save with
  `bookclerk config s3-credentials set`). DB rows fail closed if the master key
  cannot unseal them (no silent SDK fall-through). When no DB row is present,
  the AWS SDK default provider chain applies (`~/.aws/credentials`, SSO,
  EC2/ECS/EKS roles — CLI install not required). Bucket/region/endpoint/
  path-style from `BOOKCLERK_OUTPUT_S3_*` (or familiar `BOOKCLERK_S3_*`) env
  vars or `[output.s3]` in config.toml. Local output uses `[output.local]` /
  `BOOKCLERK_OUTPUT_LOCAL_ROOT`. Multiple destination plugins may be
  `enabled` at once — acquire writes to every enabled destination.
- `BOOKCLERK_S3_ENDPOINT` may be host-only (no scheme); Bookclerk prepends
  `https://` when the value looks like a bare hostname.
