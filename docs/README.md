# Bookclerk documentation

Bookclerk gathers the audiobooks you bought from several stores into one
library that you control: it syncs what you own, downloads your copies, saves
them where you choose, and connects to library servers such as Audiobookshelf.

The docs are grouped by what you want to do. Start with the first group; the
others go deeper.

## Use Bookclerk

- [Getting started](guide/getting-started.md): set up Bookclerk, connect a store, and bring in your library
- [Stores](guide/sources.md): Audible, Libro.fm, Chirp and GraphicAudio
- [Where your books go](guide/destinations.md): a folder on disk, S3 or MinIO, and how files are named
- [Discover and wishlist](guide/discovery.md): recommendations, catalog search and the wishlist
- [The web interface](guide/gui.md): screens, sign-in and the tray icon
- [Coming from Libation](guide/migration.md): import your Libation files

## Run a server

- [Running Bookclerk](server/operations.md): the daemon, systemd, Docker and logs
- [Configuration](server/configuration.md): `config.toml`, environment variables and plugin settings
- [Database](server/database.md): SQLite, PostgreSQL or Cloudflare D1, and encrypted secrets
- [Integrations and sign-in](server/integrations.md): Audiobookshelf, invites and identity providers
- [Libraries and branches](server/cluster-hosts.md): join a library, clerks, consent and the node API
- [Media worker](server/media.md): the confined process that handles audio files
- [Diagnostics](server/diagnostics.md): opt-in error reports

## Build plugins

- [Plugins](plugins/README.md): the plugin model, the guest jail, `plugin.toml`, consent and the ABI
- [Plugin registry](plugins/registry.md): crate naming, archives and installing without Rust
- [SQL contract v1](sql-contract/v1.md): the canonical SQL that database adapters implement
- [API reference](api/README.md): the plugin SDKs, the web API and the Rust crates (generated)

## How it works

- [Architecture](internals/architecture.md): how the pieces fit
- [Durable job queue](internals/jobs.md): admission, leases and crash recovery
- [Bounded storage](internals/storage-bounds.md): size caps, streams, paged scans and checkpoints
- [workerd bridge](internals/workerd-bridge.md): the wire between the launcher and a plugin isolate
- [Windows AppContainer](internals/windows-appcontainer.md): confinement notes for Windows

## Contribute

- [Dev Container](contributing/devcontainer.md): a ready-made build environment
- [Continuous integration](contributing/ci.md): the planner, `CI Gate`, fuzzing and deploys
- [Release packaging](contributing/packaging.md): platform bundles and release CI
- [Code documentation](contributing/code-documentation.md): how to document Rust, TypeScript, Python and shell
- Research notes: [store candidates](contributing/research/source-candidates.md) and [Libation parity](contributing/research/libation-parity.md)

Build and agent conventions for this repository live in
[`AGENTS.md`](../AGENTS.md).

## Design decisions

- [Plugin capabilities v3](adr/plugin-capabilities-v3.md): entrypoints, triggers, typed Cap'n Proto and `env.EVENTS`
- [Workers RPC and workerd](adr/plugin-workers-rpc-workerd.md): jail, isolation and the workerd pin (partly superseded by v3)
- [First-party identity](adr/first-party-identity.md): Owner, Administrator and Member, passkeys and OIDC
- [Plugin-provided OIDC clients](adr/plugin-oidc-clients.md): players declare identity-provider clients
- [SQL database contract](adr/sql-database-contract.md): host-resolved canonical SQL, adapter-owned lowering
- [Schema versioning](adr/schema-versioning.md): frozen schema numbers, checksums and backups
- [Control plane](adr/control-plane.md): the bootstrap boundary, host identity and cluster coordination
- [Untrusted-input fuzzing](adr/untrusted-input-fuzzing.md): every untrusted boundary ships a fuzz target
- [Security advisory automation](adr/security-advisory-automation.md): draft advisories for shipped code
