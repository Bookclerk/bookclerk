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
| **Integrations** | Audiobookshelf scan notify, claim tickets, Connect portal |
| **Plugins** | External source/integration plugins over JSON-RPC stdio |
| **Ops** | `bookclerk` CLI + `bookclerkd` daemon, Docker, systemd |

GUI is intentionally out of scope for now; the HTTP control plane is the API
surface for future UIs.

## Architecture at a glance

```text
  Storefronts              Core                            Destinations / hooks
 ─────────────            ──────                          ─────────────────────
  Audible       ─┐                                      ┌─► local filesystem
  Libro.fm      ─┼─► scan ─► library.db ─► acquire ────┼─► S3 / MinIO
  Chirp         ─┤         ▲              (+ pack)     └─► (output plugins*)
  GraphicAudio  ─┘         │
  external source*         │ naming / enrich / search
                           │
                           ├─► integrations (Audiobookshelf, Connect portal, …)
                           └─► bookclerkd jobs + HTTP control plane

  * discovered at runtime — see docs/plugins.md
```

See [docs/architecture.md](docs/architecture.md) for crate layout and data flow.

## Quick start

```bash
cargo build --release -p bookclerk-cli -p bookclerkd
export PATH="$PWD/target/release:$PATH"

export BOOKCLERK_FILES_DIR=./BookclerkFiles
mkdir -p "$BOOKCLERK_FILES_DIR"
cp config/config.example.toml "$BOOKCLERK_FILES_DIR/config.toml"

# Audible (browser / QR OAuth — OTP/2FA completed in the browser)
bookclerk auth login -m us

# Other stores (password via env — never on argv)
# export BOOKCLERK_LIBRO_PASSWORD='…'
# bookclerk auth login --source libro --email you@example.com
# export BOOKCLERK_CHIRP_PASSWORD='…'
# bookclerk auth login --source chirp --email you@example.com
# export BOOKCLERK_GA_PASSWORD='…'
# bookclerk auth login --source graphicaudio --email you@example.com

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
| [docs/integrations.md](docs/integrations.md) | Audiobookshelf, Connect portal |
| [docs/plugins.md](docs/plugins.md) | Third-party plugin protocol |
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

[output.local]
enabled = true
root = "/data/Audiobooks"

[output.s3]
enabled = false
# bucket = "my-audiobooks"
# region = "us-east-1"

[sources.audible]
enabled = true

[sources.libro]
enabled = true

[sources.chirp]
enabled = true

[sources.graphicaudio]
enabled = true
access = "web"          # web | zip | device

[integrations.audiobookshelf]
enabled = false
# base_url = "http://audiobookshelf:80"
```

S3 credentials stay in the environment (`AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY`). Store passwords use per-source env vars (see
[docs/sources.md](docs/sources.md)).

## Binaries

| Binary | Role |
| --- | --- |
| `bookclerk` | One-shot CLI (`auth`, `library`, `integrations`, `plugins`, …) |
| `bookclerkd` | Long-running daemon: scheduled scan/acquire + HTTP control plane |

Default control plane: `127.0.0.1:8787` — `GET /health`, `GET /status`,
`POST /scan`, `POST /acquire`, `GET /jobs` (JSON bodies need
`Content-Type: application/json`).

## Status

Headless multi-source acquire, daemon, destinations, Audiobookshelf
integration, and the plugin host are in active use. Libation Classic/Chardonnay
CLI parity for the Audible acquire surface is tracked in
[docs/libation-parity.md](docs/libation-parity.md). A native/web GUI is deferred.

## License

GPL-3.0-or-later (aligned with upstream Libation). Dependencies such as
[audible-rs](https://github.com/mkb79/audible-rs) remain under their own
licenses (MIT).
