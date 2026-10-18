# AGENTS.md

Instructions for anyone, human or agent, changing this repository. The
contributing workflow and review guidelines come first; environment, build and
product facts follow.

## Contributing workflow

### Scope

Every pull request states its intent and scope in its description (the
[pull request template](.github/pull_request_template.md) has the headings).
Changes and review fixes stay inside that scope.

### Code standards

Write code that passes the lints the first time; CI turns every warning into
an error (`RUSTFLAGS="-D warnings"`).

Documentation is enforced by `missing_docs` and
`clippy::missing_docs_in_private_items`, by `# Errors` / `# Panics` sections on
the publish crates, by TypeDoc and lint checks on the TypeScript SDK and
`ui/src/lib`, and by Google-style docstring checks on the Python SDK:

- Document **every** item, public and private, including struct fields and enum
  variants, in the style described in
  [`docs/code-documentation.md`](docs/code-documentation.md).
- Start with a one-sentence summary, then details. Say what the value means,
  its units and its invariants; never restate the identifier (`/// Path.` is
  rejected).
- Functions get `# Arguments`, `# Returns`, `# Errors` and `# Panics` whenever
  they are not obvious from the signature.
- No crate- or module-wide `allow(missing_docs)`; narrow allows only for
  generated code that cannot carry comments.

Types:

- Encode meaning in types: newtypes for ids, keys, units and secrets instead of
  bare `String`, `u64` or `Vec<u8>`; enums instead of string or boolean flags;
  parse untrusted input into a validated type at the boundary.
- No lossy `as` casts on sizes, offsets or durations (use `try_from`) and no
  `unwrap` / `expect` outside tests. Where an invariant truly guarantees
  success, use a narrow `#[allow(clippy::expect_used)]` with the reason in a
  comment, and don't add new sites under a crate-level allow.
- Exhaustive `match` on project enums (no catch-all `_` that would hide a new
  variant).
- TypeScript is `strict` with no `any`; Python has type hints on every
  function.
- Run `cargo fmt` before committing rather than formatting by hand.

### Before every push

- `cargo fmt --all -- --check` and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Tests for what changed: `python3 scripts/ci-plan.py --base origin/main --head
  HEAD --format summary` shows the plan; `python3 scripts/ci-exec.py resolve
  --selective 1 --base origin/main --head HEAD --run-id local` writes it, then
  `python3 scripts/ci-exec.py run <check> --run-id local` runs each planned
  check. Run `cargo test --workspace` when the plan says full suite.
- Where available: the cross-platform CI emulation for platform-specific
  changes, a dependency vulnerability scan when a lockfile changed, and static
  analysis for security-surface changes. Where they are not available, CI runs
  them.
- Review your own diff against `origin/main`, including a security pass for
  security-surface changes, and fix what you confirm.

### Pull requests

- Open pull requests as drafts. Mark ready only when the `CI Gate` check is
  green and no review thread is unresolved.
- Every push is reviewed again by automated reviewers. Let a round of reviews
  land, then address it in one push.
- Answer every review thread with the fixing commit, the follow-up issue, or
  why it stays, then resolve it.
- Never skip or disable a test, push an empty commit, or close and reopen a
  pull request to re-run CI.

### Security surface

`bookclerk-jail`, `bookclerk-sandbox`, `bookclerk-workerd`,
`bookclerk-plugin-host`, `bookclerk-plugin-abi` (`plugin.capnp`),
`bookclerk-db-exec`, `docs/sql-contract/`, `bookclerk-media-worker`,
`bookclerk-mp4`, secrets / `master.key` / operator-token code, and
`.github/workflows/`.

## Review guidelines

### Judge against the declared scope

Read the pull request's **Intent**, **In scope** and **Out of scope** first,
and review the change against them:

- Does the change do what the intent says, completely? Missing pieces of the
  stated scope are findings.
- Does it change anything the scope does not cover? Unexplained changes are
  findings.
- Does it break behavior outside its files? That is in scope.

Findings that are substantially different from the pull request's intent
(pre-existing problems, other features, redesigns, "while you're here"
improvements) do not belong in the pull request. Mark them as out of scope.
The author opens an issue for each one (labeled `review-followup`, linking the
review comment) and addresses it in a follow-up pull request. A real security
problem found out of scope still gets an issue right away. If a finding shows
the declared scope itself is wrong, the author updates the scope in the
description rather than widening the change silently.

### Invariants (blocking)

- No in-process fallback for plugins, database adapters, or codecs; missing
  helpers fail closed (`media.isolation` / `plugins.isolation = "required"`).
- Secrets never on argv, in env vars forwarded to guests, or in logs; new
  secret paths register with redaction.
- Guests get only their declared capabilities; network access only to approved
  `[capabilities.network]` domains.
- Host SQL goes through the canonical SQL-v1 contract
  ([`docs/sql-contract/v1.md`](docs/sql-contract/v1.md)), never engine-specific
  SQL.
- `plugin.capnp` is append-only (new ordinals only) and changes ship with the
  TS SDK.
- Plugin configuration lives in the database; no new `[sources.*]`-style TOML
  tables or `BOOKCLERK_*` env overrides.
- Prefer a plugin entrypoint or job/event over host special-casing.

### Low priority

Formatting and lints already enforced by rustfmt and Clippy.

## Environment and services

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
| `.cargo-home/` | `CARGO_HOME` (registry + git) on the **Dev Container / Cloud** bind mount (`remoteEnv` / `cloud-agent-install.sh`). Local host and local Cursor use default `~/.cargo` so they share a registry. |
| `target/` | `CARGO_TARGET_DIR` (also `[build].target-dir` in `.cargo/config.toml`) |
| `.tmp/` | `TMPDIR` for `cc` / build-script temps (avoids host `/tmp` quota) |
| `BookclerkFiles/` | default `$BOOKCLERK_FILES_DIR` for `cargo dev` |

`cargo reset --yes` wipes `BookclerkFiles/` (not `target/` / `.cargo-home/`).

Product docs live under [`docs/`](docs/README.md). Bookclerk is a
**multi-storefront** audiobook library manager (sources + destinations +
integrations), not an Audible-only Libation fork. Public APIs use
[Google-style inline docs](docs/code-documentation.md); regenerate HTML with
`./scripts/generate-api-docs.sh` into `docs/api/`.

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
- `bookclerk-workerd` — the front door every plugin is spawned through: loads
  `runtime = "workerd"` isolates and fronts `runtime = "native"` guests as a
  sibling jail joined by inherited `fd:` / `handle:` links; ships beside
  hosts with the pinned `workerd` binary, and both are required in every
  isolation mode (direct native spawn is
  `SpawnTransport::DirectNativeDiagnostic`, tests only).

Optional companion (workspace member, not a default-member):

- `bookclerk-tray` — in-process tray library linked into `bookclerkd` (opens the
  web UI in the browser; Linux `ksni`; Windows/macOS `tray-icon` with GTK
  features off). See `docs/gui.md`.

Frontend sources live in `ui/` (Vite/React); `cargo dev` / `cargo build-app --platform`
rebuild `ui/dist` when SPA sources are newer than the last Vite output. Manual:
`npm ci && npm run build` in `ui/` so `bookclerkd` can serve `ui/dist`. Do not add Tauri/GTK3-pinned shells while
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
- CI planner (selective PR checks): `python3 scripts/ci-plan.py --paths <file>… --format summary`
  (or `--base <sha> --head <sha>`); reproduce a check locally with
  `python3 scripts/ci-exec.py resolve --selective 1 --paths <file>…` then
  `python3 scripts/ci-exec.py run <check> --run-id local`. Tests:
  `python3 scripts/tests/test_ci_plan.py`, `test_ci_relations.py`, and
  `test_ci_contract.py` (needs PyYAML from `scripts/tests/requirements.txt`).
  Relationships Cargo cannot see (embedded SDK files, cross-package test
  inputs, executables tests launch) go in `scripts/ci_plan/relations.toml` —
  see [`docs/ci.md`](docs/ci.md). Branch protection should require the stable
  **`CI Gate`** job (not every matrix child).
- Release binaries: `cargo build --release -p bookclerk-cli -p bookclerkd
  -p bookclerk-media-worker -p bookclerk-jail -p bookclerk-workerd`
  (helpers are not optional — see above)
- Confinement tests no-op without a backend, so demand enforcement when it is
  expected: `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT=1 cargo test --workspace`
  (CI sets this on Linux/macOS for self-confine tests; Windows also sets
  `BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT=1` for AppContainer jail/media).
  Plugin-host tests also launch `bookclerk-jail`, `bookclerk-workerd` and the
  sqlite/postgres guests, which `cargo test --workspace` builds but
  `cargo test -p bookclerk-plugin-host` does not (`cargo build -p …` them
  first; `BOOKCLERK_REQUIRE_TEST_GUESTS=1` turns a missing guest into a failure).

### External plugins (local dev)

Hosts default to **external guests only** (jailed subprocesses — including
platform `sqlite` / `local`). Product storefronts are optional; reference Echo
examples under `examples/` are CI/dev-only (never packaged).

```bash
cargo reset --yes             # wipe BookclerkFiles/ (stale DB / config)
cargo dev                     # platform: hosts + helpers + sqlite/local + UI, then bookclerkd
cargo dev --optional          # also build/stage optional storefronts
cargo dev --examples          # also stage reference Echo examples
cargo dev --skip-build        # install/stage + exec when target/ is already warm
cargo dev-cli -- version      # same platform build, then CLI
cargo build-app --platform --optional --examples
cargo ensure-workerd          # pinned Cloudflare workerd beside helpers
cargo install-platform --skip-build
cargo stage-plugins --optional --examples --skip-build
cargo stage-plugins --plugin libro --skip-build   # individual guest(s) by plugin.toml id
cargo test-staged             # describe/health conformance (bookclerk-plugin-e2e) against installed + staged guests
cargo test-staged --plugin libro                  # platform + only libro
cargo plugin -- check <dir>   # author CLI (crate bookclerk-plugin-tools)
```

See `docs/plugins.md` and `crates/bookclerk-dev/README.md`.

`cargo dev` defaults `BOOKCLERK_FILES_DIR` to `<workspace>/BookclerkFiles`
(override with the env var). On first use the app creates `library.db` (SQLite
by default via the `[database]` plugin — see `docs/database.md`; Cloudflare D1
optional), plus `cache/`, `logs/` (reserved; Bookclerk does not rotate log
files), `search_index/`, and `plugins/` under it. Third-party plugins are discovered from `plugin.toml` under
`plugins/` (and `BOOKCLERK_PLUGIN_DIRS`); enablement and knobs live in
`config.toml` (see `docs/plugins.md`). A guest is confined to its install
directory (read-only), `plugin-state/<PluginKey fs-id>/data` (its `HOME`), `plugin-state/<PluginKey fs-id>/tmp` (its
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
  Examples: `version`, `library accounts`, `library list`.
- Daemon: `cargo run -p bookclerkd` (or `cargo dev`). Listens on `127.0.0.1:8787`
  and `[::1]:8787` by default (override with `BOOKCLERK_DAEMON_LISTEN` or
  `daemon.listen` in `config.toml`; string, array, or comma-separated). Operator
  auth defaults on (token sealed in `encrypted_secrets`, or
  `BOOKCLERK_OPERATOR_TOKEN`). Control plane: `GET /health`,
  `POST /api/auth/login`, authenticated `/api/status`, `/api/jobs`,
  `/api/library/*` (legacy `/status` `/scan` `/acquire` `/jobs` also gated).
  `POST` bodies require the `Content-Type: application/json` header (send `{}`
  for defaults), otherwise the request is rejected. Show/rotate the Bearer token
  with `bookclerk daemon token` / `bookclerk daemon token rotate`. Print a
  loopback operator sign-in URL with `bookclerk login` (requires a running
  daemon).

### Live store / storage testing constraints

When exercising real store credentials in this cloud environment:

- Connect stores via the **User SPA Accounts** UI (administrator/member session
  from a claim ticket or integration login), not the removed `bookclerk auth`
  CLI. Prefer interactive OAuth/password connect in Accounts when testing
  Audible login itself (not a pre-baked `.auth` file).
- Amazon accounts with **2FA/MFA require OTP** during the browser OAuth step
  (audible-rs has no password CLI). Use a TOTP seed or complete the challenge
  in the Desktop pane; see README / `crates/bookclerk-plugins/optional/source-audible/README.md`.
- Password stores: enter credentials in Accounts (never put passwords on argv).
  Guests receive the stored password as a sealed secret; no
  `BOOKCLERK_*_PASSWORD` env var is read by Bookclerk or forwarded to a guest.
- Keep `library.auto_acquire = false` in `$BOOKCLERK_FILES_DIR/config.toml`.
- After login, **disable the account for scans**:
  `bookclerk library set-scan <account> --scan false`.
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
  Libation AudibleCdm (Accounts connect registers as Android); optional BYO
  `.wvd` still works. Spatial/Atmos (L1) is not available. Neither a CDM nor
  ffmpeg is required to build, test, or run non-acquire commands.
- Plugin configuration is **database only**. A plugin declares its settings in
  `plugin.toml` `[[settings]]` and a semver `version`; enrollment pins that
  schema under the version (newest wins and documents are reconciled to it on every enrollment pass; an
  older installed package is not enrolled or spawned) and creates the
  default instance; `bookclerk plugins config get|set|unset <plugin> <key>`
  edits the instance document (`--secret` reads the value from stdin and
  seals it); `bookclerk plugins deploy|undeploy <plugin> [--node <id>]` sets
  the deployment (`present` = runs on that node). `bookclerk plugins deploy
  <plugin> --replicas N|all --on <label>` hands an instance to the cluster
  scheduler instead, which writes `placed`/`unplaced` rows on the live nodes
  with the most budget headroom (each daemon heartbeats capacity, budget,
  reservations, and use into `node_status` every 15 s; budgets and drain
  via `bookclerk nodes set`, labels via `bookclerk nodes label`);
  per-node deploy/undeploy then refuse it until `plugins deploy --manual`.
  `bookclerk plugins install` records a cluster-wide installed version (nodes
  materialize it under `plugins/.versions/<digest>`); it no longer writes
  `plugins/<fs-id>`. There are no `[sources.*]`,
  `[output.local]`, `[output.s3]`, or `[integrations.<id>]` tables and no
  `BOOKCLERK_SOURCE_*` / `BOOKCLERK_OUTPUT_*` / `BOOKCLERK_S3_*` /
  `BOOKCLERK_AWS_*` / `BOOKCLERK_ABS_*` env overrides. Only `[database]`,
  `master.key`, `node-identity.json`, and `daemon.listen` stay bootstrap.
- S3/MinIO: `bucket`, `region`, `endpoint`, `force_path_style`, `prefix` and
  the `access_key_id` / `secret_access_key` / `session_token` secrets are the
  `s3` plugin's settings. With no sealed keys the guest uses the AWS SDK
  default provider chain. `endpoint` may be host-only; `https://` is
  prepended for a bare hostname. Local output root is the `local` plugin's
  `root` setting. Every destination deployed on the host receives each
  acquire.
- Accounts are keyed `(plugin_key, account_id)` and are not tied to a plugin
  instance or host. `library set-scan` / `revoke-account` take `--source`
  when an id exists on two plugins; `library scan|acquire --instance` pins a
  non-default instance.
