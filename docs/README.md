# Bookclerk documentation

Bookclerk is a **multi-storefront audiobook library manager**: sync owned
libraries from several stores, acquire and decrypt media, write to one or more
destinations, and notify or connect external library servers.

Start here if you are new:

1. [Getting started](getting-started.md) — build, auth, first scan/acquire
2. [Architecture](architecture.md) — how the pieces fit
3. [Sources](sources.md) — Audible, Libro.fm, Chirp, GraphicAudio
4. [Destinations](destinations.md) — local disk and S3/MinIO
5. [Integrations](integrations.md) — Audiobookshelf and the Connect portal
6. [Configuration](configuration.md) — `config.toml` and environment variables
7. [Operations](operations.md) — `bookclerkd`, Docker, systemd
8. [GUI](gui.md) — web UI, operator auth, tray companion
9. [Desktop path](gui-desktop-path.md) — tray vs deferred Tauri / OSV constraints

## Extending Bookclerk

| Doc | When to read |
| --- | --- |
| [Plugins](plugins.md) | Ship or host a third-party source/integration |
| [Source candidates](source-candidates.md) | Research notes for stores not yet implemented |
| [Diagnostics](diagnostics.md) | Opt-in crash/error reporting pipeline |

## Compatibility

| Doc | When to read |
| --- | --- |
| [Migration](migration.md) | Moving from classic Libation Files |
| [Libation parity](libation-parity.md) | Headless CLI/settings matrix vs Libation Chardonnay |

## Agent / contributor notes

Cloud-agent and local-dev conventions live in [`AGENTS.md`](../AGENTS.md) at
the repository root (build commands, live-credential constraints, gotchas).
