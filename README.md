# Bookclerk

**Every purchase, properly placed.**

Bookclerk is a headless-first **multi-storefront audiobook library manager**.
It syncs owned titles from multiple stores into one library, acquires and
decrypts media with a native pipeline, writes to local disk and/or object
storage, and hooks into library servers through pluggable integrations.

It began as a Rust rewrite of [Libation](https://github.com/rmcrackan/Libation)
for Audible. It is now a broader toolkit: several content sources, multiple
output destinations, a daemon control plane, third-party plugins, and a Connect
portal for services like [Audiobookshelf](https://www.audiobookshelf.org/).

## What you get

| Area | Capabilities |
| --- | --- |
| **Sources** | Audible, Libro.fm, Chirp, GraphicAudio (enable/disable per store) |
| **Library** | Unified SQLite catalog (UUID keys; ASIN/ISBN as attributes), Tantivy search, tags/ratings |
| **Acquire** | Native Adrm + Widevine/CENC decrypt, optional MP3, covers/PDFs/cues/chapters |
| **Destinations** | Local filesystem and/or S3/MinIO (write to every enabled destination) |
| **Integrations** | Audiobookshelf scan notify, claim tickets, SPA Accounts |
| **Plugins** | External source/integration plugins over Workers RPC (`api_version = 3`), each in a host-imposed jail |
| **Ops** | `bookclerk` CLI + `bookclerkd` daemon, Docker, systemd |
| **GUI** | Shared React web UI served by `bookclerkd` (native/tray deferred) |

The library GUI and `/api/*` control plane live in Rust (`bookclerkd`) with
operator-token auth. See [docs/gui.md](docs/gui.md).

## Architecture at a glance

```text
  Storefronts              Core                            Destinations / hooks
 ─────────────            ──────                          ─────────────────────
  Audible       ─┐                                     ┌─► local filesystem
  Libro.fm      ─┼─► scan ─► library.db ─► acquire ────┼─► S3 / MinIO
  Chirp         ─┤         ▲              (+ pack)     └─► (output plugins*)
  GraphicAudio  ─┘         │
  external source*         │ naming / enrich / search
                           │
                           ├─► integrations (Audiobookshelf, SPA Accounts, …)
                           └─► bookclerkd jobs + HTTP control plane

  * discovered at runtime — see docs/plugins.md
```

See [docs/architecture.md](docs/architecture.md) for crate layout and data flow.

## Quick start

```bash
cargo build --release -p bookclerk-cli -p bookclerkd \
  -p bookclerk-media-worker -p bookclerk-jail
export PATH="$PWD/target/release:$PATH"

export BOOKCLERK_FILES_DIR=./BookclerkFiles
mkdir -p "$BOOKCLERK_FILES_DIR"
cp config/config.example.toml "$BOOKCLERK_FILES_DIR/config.toml"

# Connect stores in the web UI (Accounts): Audible via browser / QR OAuth
# (OTP/2FA completed in the browser); Libro.fm, Chirp and GraphicAudio via
# email + password, sealed into the database. Passwords are never read from
# the environment or argv.
bookclerk login   # prints a loopback sign-in URL for a running bookclerkd

bookclerk library scan
bookclerk library acquire --asin B0EXAMPLE   # or --isbn / UUID / product id
```

First-time setup, auth notes, and “existing files” workflows:
[docs/getting-started.md](docs/getting-started.md).

## Documentation

| Doc | Topic |
| --- | --- |
| [docs/README.md](docs/README.md) | Documentation index |
| [docs/getting-started.md](docs/getting-started.md) | Install, auth, first scan/acquire |
| [docs/architecture.md](docs/architecture.md) | Components and data flow |
| [docs/sources.md](docs/sources.md) | Storefront plugins and login |
| [docs/destinations.md](docs/destinations.md) | Local + S3 output, naming |
| [docs/integrations.md](docs/integrations.md) | Audiobookshelf, SPA Accounts |
| [docs/plugins.md](docs/plugins.md) | Third-party plugin protocol and the guest jail |
| [docs/operations.md](docs/operations.md) | Daemon, Docker, systemd |
| [docs/configuration.md](docs/configuration.md) | `config.toml` and env overrides |
| [docs/migration.md](docs/migration.md) | Move from classic Libation |
| [docs/diagnostics.md](docs/diagnostics.md) | Opt-in crash/error reports |
| [docs/libation-parity.md](docs/libation-parity.md) | Headless Libation parity matrix |
| [docs/source-candidates.md](docs/source-candidates.md) | Research notes for future stores |

## Example configuration

Minimal multi-store, local + S3 sketch (full example:
[`config/config.example.toml`](config/config.example.toml)):

```toml
[library]
auto_acquire = false
scan_interval_minutes = 60
enrich_from_audible = true

[output]
format = "enriched_m4b"
naming_profile = "audiobookshelf"
```

`[output]` only seeds the shared `core.output` document the first time a host
enrolls against the database; after that change it with
`bookclerk config set output.<key> <value>` or **Settings → Output**.

Storefronts, destinations, and integrations are plugin instances in the
database, not TOML. Each plugin declares its settings in `plugin.toml`
`[[settings]]`; "enabled" is a deployment on a host:

```bash
bookclerk plugins list
bookclerk plugins config set local root /data/Audiobooks
bookclerk plugins config set s3 bucket my-audiobooks
bookclerk plugins config set s3 secret_access_key --secret   # value on stdin
bookclerk plugins deploy s3
bookclerk plugins config set graphicaudio access web          # web | zip | device
bookclerk plugins deploy audiobookshelf
```

The same operations are in **Settings → Plugins**. Store passwords are
entered in Accounts and sealed in the database (see
[docs/sources.md](docs/sources.md)).

## Binaries

| Binary | Role |
| --- | --- |
| `bookclerk` | One-shot CLI (`auth`, `library`, `integrations`, `plugins`, …) |
| `bookclerkd` | Long-running daemon: scheduled scan/acquire + HTTP API / GUI (optional in-process tray) |
| `bookclerk-media-worker` | Confined child process for one codec job ([docs/media.md](docs/media.md)) |
| `bookclerk-jail` | Confines a plugin guest, then becomes it ([docs/plugins.md](docs/plugins.md#the-guest-jail)) |

The two helpers ship beside the hosts. Without them, a host refuses media work
and declines to load external plugins rather than running either unconfined.

Default listen: `127.0.0.1:8787` and `[::1]:8787`. Public routes: `GET /health`, static UI,
`POST /api/auth/login`. Authenticated: `/api/status`, `/api/jobs`,
`/api/library/*`, plus legacy `/status` `/scan` `/acquire` `/jobs`.
JSON POST bodies need `Content-Type: application/json`.

## Status

Headless multi-source acquire, daemon, destinations, Audiobookshelf
integration, the plugin host, an MVP web library GUI, and an optional
in-process tray→browser companion are in active development. An embedded Tauri
window is deferred until an OSV-clean GTK4 graph is available (see
[docs/gui.md](docs/gui.md)). Libation Classic/Chardonnay CLI parity for the
Audible acquire surface is tracked in
[docs/libation-parity.md](docs/libation-parity.md).

## License

GPL-3.0-or-later (aligned with upstream Libation). Dependencies such as
[audible-rs](https://github.com/mkb79/audible-rs) remain under their own
licenses (MIT).
