# Bookclerk documentation

Bookclerk gathers the audiobooks you bought from several stores into one
library that you control: it syncs what you own, downloads your copies, saves
them where you choose, and connects to library servers such as Audiobookshelf.

The docs are grouped by what you want to do. Start with the first group; the
others go deeper.

## Use Bookclerk

- [What is Bookclerk?](guide/what-is-bookclerk.md): what it does, whether it is ready, and how to try the demo
- [Getting started](guide/getting-started.md): install Bookclerk, create your account, connect a store and bring in your books
- [Stores](guide/stores.md): Audible, Libro.fm, Chirp and GraphicAudio
- [Where your books go](guide/where-books-go.md): folders, cloud storage and file names
- [Discover and wishlist](guide/discover.md): suggestions, catalog search and the wishlist
- [The web app](guide/web-app.md): the screens, roles and ways to sign in
- [Sharing with your household](guide/sharing.md): invites, what is shared, and Audiobookshelf
- [Join a library as a branch](guide/branches.md): let a second computer help
- [Coming from Libation](guide/from-libation.md): bring over an existing library
- [Troubleshooting](guide/troubleshooting.md): common problems and fixes
- [Glossary](guide/glossary.md): the words Bookclerk uses

## Run a server

- [Running Bookclerk](server/operations.md): the daemon, systemd, Docker and logs
- [Configuration](server/configuration.md): `config.toml`, environment variables and plugin settings
- [Security checklist](server/security.md): keys, TLS, sign-in and isolation before you expose a server
- [Database](server/database.md): SQLite, PostgreSQL or Cloudflare D1, and encrypted secrets
- [Stores reference](server/stores.md): store plugins, formats and settings
- [Output and destinations reference](server/output.md): local and S3 destinations, naming and packaging
- [Web app and sign-in reference](server/sign-in.md): roles, sign-in methods, routes and screens
- [Integrations and sign-in](server/integrations.md): Audiobookshelf, invites and identity providers
- [Managing plugins](server/plugins.md): install, approve, configure and place plugins; isolation modes
- [Libraries and branches](server/cluster-hosts.md): join a library, clerks and consent
- [Media worker](server/media.md): the confined process that handles audio files
- [Backups, updates, import and export](server/backups.md): native backups, schema backups, updating, Libation and PostgreSQL export
- [Diagnostics](server/diagnostics.md): opt-in error reports
- [CLI reference](server/cli-reference.md): every built-in `bookclerk` command and option (generated)

## Build plugins

- [Plugins](plugins/README.md): the plugin model, SDKs, local development and examples
- [plugin.toml](plugins/manifest.md): package layout, identity, entrypoints and settings
- [Capabilities and consent](plugins/capabilities.md): what a plugin may ask for and how it is approved
- [Entrypoints, events and the ABI](plugins/abi.md): the Workers RPC surface, events, KV and queues
- [Plugin registry](plugins/registry.md): crate naming, archives and installing without Rust
- [SQL contract v1](sql-contract/v1.md): the canonical SQL that database adapters implement
- [API reference](api/README.md): the plugin SDKs, the web API and the Rust crates (generated)

## How it works

- [Architecture](internals/architecture.md): how the pieces fit
- [Discovery design](internals/discovery.md): how suggestions are found and ranked
- [Durable job queue](internals/jobs.md): admission, leases and crash recovery
- [Bounded storage](internals/storage-bounds.md): size caps, streams, paged scans and checkpoints
- [The guest jail](internals/guest-jail.md): why plugins run as jailed processes, and how each platform confines them
- [workerd bridge](internals/workerd-bridge.md): the wire between the launcher and a plugin isolate
- [Node API and the live socket](internals/node-api.md): how hosts of one library talk, and how the web app hears about changes
- [Windows AppContainer](internals/windows-appcontainer.md): confinement notes for Windows

## Contribute

- [Build from source and use the CLI](contributing/build-from-source.md): build the binaries and drive Bookclerk from the command line
- [Dev Container](contributing/devcontainer.md): a ready-made build environment
- [Web UI development](contributing/ui.md): the React app, the tray and brand assets
- [Continuous integration](contributing/ci.md): the planner, the jobs and `CI Gate`
- [Fuzzing](contributing/fuzzing.md): fuzz targets in CI and crash triage
- [Repository automation](contributing/automation.md): dependency updates, advisories, docs publishing and Worker deploys
- [Release packaging](contributing/packaging.md): platform bundles and release CI
- [Code documentation](contributing/code-documentation.md): how to document Rust, TypeScript, Python and shell
- [Writing docs](contributing/writing-docs.md): how to write a docs page people can read quickly
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
- [Logging and tracing](adr/observability.md): trace everything, keep TRACE for diagnostics, show each output its own level
