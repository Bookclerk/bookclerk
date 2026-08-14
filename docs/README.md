# Bookclerk documentation

Bookclerk is a **multi-storefront audiobook library manager**: sync owned
libraries from several stores, acquire and decrypt media, write to one or more
destinations, and notify or connect external library servers.

Start here if you are new:

1. [Getting started](getting-started.md) — build, auth, first scan/acquire
2. [Architecture](architecture.md) — how the pieces fit
3. [Sources](sources.md) — Audible, Libro.fm, Chirp, GraphicAudio
4. [Destinations](destinations.md) — local disk and S3/MinIO
5. [Database](database.md) — SQLite default, Cloudflare D1, SeaORM
6. [Integrations](integrations.md) — Audiobookshelf and SPA claim / Accounts
7. [Discovery](discovery.md) — recommendations, embeddings, wishlist / global queue
8. [Configuration](configuration.md) — `config.toml` and environment variables
9. [Media worker pool](media.md) — confined codecs, concurrency, isolation modes
10. [Operations](operations.md) — `bookclerkd`, Docker, systemd
11. [Dev Container](devcontainer.md) — consistent Rust/OpenSSL/Node build env (Cursor / VS Code)
12. [GUI](gui.md) — web UI, operator auth, tray companion
13. [Desktop path](gui-desktop-path.md) — tray vs deferred Tauri / OSV constraints
14. [Continuous integration](ci.md) — dependency-aware planner, selective docs, `CI Gate`

## Extending Bookclerk

| Doc | When to read |
| --- | --- |
| [Plugins](plugins.md) | Ship or host a third-party source/integration (Workers RPC ABI, jail, workerd) |
| [ADR: Workers RPC + workerd](adr/plugin-workers-rpc-workerd.md) | Greenfield `api_version = 1` decision: ABI, `BookclerkPlugin`, consent |
| [ADR: First-party identity](adr/first-party-identity.md) | Operator / Owner / Administrator / Member, optional multi-IdP broker, passkeys, OIDC for ABS |
| [Plugin registry](plugin-registry.md) | crates.io taxonomy, native vs workerd archives, catalog roadmap |
| [Packaging](packaging.md) | `cargo package-*` aliases, platform bundles, release CI |
| [Source candidates](source-candidates.md) | Research notes for stores not yet implemented |
| [Diagnostics](diagnostics.md) | Opt-in crash/error reporting pipeline |
| [Windows AppContainer](windows-appcontainer.md) | Job ordering, ACL sync, OAuth callback IPC, availability |

## Compatibility

| Doc | When to read |
| --- | --- |
| [Migration](migration.md) | Moving from classic Libation Files |
| [Libation parity](libation-parity.md) | Headless CLI/settings matrix vs Libation Chardonnay |
| [ADR: Workers RPC + workerd](adr/plugin-workers-rpc-workerd.md) | Plugin ABI target state (not Cap’n Proto / not legacy JSON-RPC product ABI) |

## API reference (generated)

| Doc | When to read |
| --- | --- |
| [Code documentation (Google style)](code-documentation.md) | Inline `///` / JSDoc / Python docstring conventions |
| [Generated API reference](api/README.md) | How to build rustdoc / TypeDoc / pdoc into `docs/api/` |

```bash
./scripts/generate-api-docs.sh
```

## Agent / contributor notes

Cloud-agent and local-dev conventions live in [`AGENTS.md`](../AGENTS.md) at
the repository root (build commands, live-credential constraints, gotchas).
