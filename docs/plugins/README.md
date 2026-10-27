# Dynamic plugins

This shelf is for people writing Bookclerk plugins. This page gives the
overview: what a plugin is, how it runs, which SDK to use, and how to try
one locally.

## What a plugin is

Bookclerk is built around pluggable **sources**, **destinations** and
**integrations**.

- First-party **guest** implementations live under
  `crates/bookclerk-plugins/{platform,optional}/`, one package per plugin.
- The **host** runtime is
  [`bookclerk-plugin-host`](../../crates/bookclerk-plugin-host): discovery,
  spawn, jail, consent and Workers RPC. It is distinct from the runtime
  install tree `$BOOKCLERK_FILES_DIR/plugins/`.

Every plugin, first-party or not, runs as a separate guest process
through `bookclerk-workerd`. There is no in-process fallback.

## Pages on this shelf

- [plugin.toml](manifest.md): package layout, identity, runtime,
  entrypoints and settings.
- [Capabilities and consent](capabilities.md): what a plugin may ask for,
  and how the operator approves it.
- [Entrypoints, events and the ABI](abi.md): the Workers RPC surface,
  events, KV, queues and every capability's methods.
- [Plugin registry](registry.md): crate naming, release assets and the
  planned registry.
- [SQL contract v1](../sql-contract/v1.md): what a database adapter must
  accept.

Operators install, approve and place plugins as described in
[Managing plugins](../server/plugins.md). How the host confines every guest
is in [The guest jail](../internals/guest-jail.md).

## Write a plugin

Authors write a Workers-shaped module:

- The default export extends **`BookclerkEntrypoint`** and implements the
  triggers `plugin.toml` declares:
  - `event(batch)` for `[[events.consumers]]`;
  - `job(job)` for `[triggers] jobs`;
  - `databaseMigrations(binding)` for `[[databases]]`;
  - plus an optional `describe()` refinement.
- Every named entrypoint in `entrypoints = [...]` is an exported class
  with the matching name, extending its base: `Storefront`, `Storage`,
  `RemoteLibrary`, `DatabaseAdapter`, `Cli` or `Oidc`.
- Native guests implement the Rust `PluginWorker`
  (`describe` / `open(invocation, bindings) -> Entrypoints`) and call
  `serve`. On native guests, `serve` is the stdin/stdout Cap'n Proto
  runner for `api_version = 3`.

The trusted adapter isolate installs the granted bindings on `env` for
each invocation: `CONFIG`, `SECRETS`, `EVENTS`, `WORK_FS` and named
databases. It never exposes HTTP endpoints, PIDs, credentials or Cap'n
Proto to authors.

- Native guests behind workerd are reached as typed Cap'n Proto by the
  launcher, never through an isolate binding.
- Byte `Source` is the job input opener. Storefronts are the separately
  named `storefront` entrypoint.
- JSON is allowed only for plugin-specific extensible config
  (`schemaVersion` plus `mediaType`/`schemaId`, and a bounded payload).

### SDKs

| Language | Package | Notes |
| --- | --- | --- |
| TypeScript | [`@bookclerk/plugin-sdk`](../../packages/plugin-sdk) | `/workerd` exports `BookclerkEntrypoint` + `*Entrypoint` bases; `bookclerk-plugin types` generates `Env` from `plugin.toml` |
| Python | [`bookclerk-plugin-sdk`](../../packages/plugin-sdk-python) | `from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, …`; module-level `Default` + named classes |
| Rust | [`bookclerk-plugin-sdk`](../../crates/bookclerk-plugin-sdk) | `PluginWorker` + `serve`; workerd guests pair `BookclerkEntrypoint` JS glue with Wasm dispatch |

Guest binaries depend on **`bookclerk-plugin-sdk`** (plus, for first-party
plugins, their private store crate). TypeScript workerd guests depend on
**`@bookclerk/plugin-sdk`**.

> [!IMPORTANT]
> Third-party authors should depend on the SDK only, not on
> `bookclerk-plugin-host`, `bookclerk-library` or `bookclerk-source`.

### Runtimes

`plugin.toml` names one of two runtimes:

| `runtime` | How it runs |
| --- | --- |
| `native` | An OS binary speaking the same Workers RPC ABI. The host starts two sibling jails, `bookclerk-workerd` (control plane) and the native backend, joined by inherited duplex links |
| `workerd` | Author `modules/` loaded by first-party [`bookclerk-workerd`](../../crates/bookclerk-workerd): **one jail and one isolate per plugin** (a local embed, not Cloudflare cloud execution) |

## How the ABI works

The product ABI is **object-capability Workers RPC** at
`api_version = 3`: capability entrypoints and triggers, transferred
`ReadableStream` and Cap'n Proto byte sources, and no public `handleId`,
`writeChunk` or base64 media.

- Native guests serve the Bookclerk Cap'n Proto schema on stdio. Workerd
  guests keep isolate `RpcTarget` stubs.
- Each HTTP or RPC request is **invocation-scoped**. The trusted adapter
  opens the guest, invokes the method, and disposes the stubs before the
  request completes.
- Survival across suspend is checkpoint data plus stable binding
  identifiers. It is never an in-memory `RpcTarget`, an adapter map id, a
  PID or an open connection.

The authoritative artifacts are:

- the Cap'n Proto
  [`schema/plugin.capnp`](../../crates/bookclerk-plugin-abi/schema/plugin.capnp)
  (append-only ordinals; unknown union members fail closed or return a
  typed `unsupported`);
- the TypeScript
  [`packages/plugin-sdk/src/plugin.ts`](../../packages/plugin-sdk/src/plugin.ts).

The decision records are
[ADR: Plugin capabilities v3](../adr/plugin-capabilities-v3.md) (the v3
surface) and
[ADR: Workers RPC and workerd](../adr/plugin-workers-rpc-workerd.md) (the
jail and the workerd front door).

## Terminology

- **Plugin host**: Crate `bookclerk-plugin-host`. Loads guests, and never ships storefront logic by default.
- **External guest**: Any jailed subprocess plugin (platform, product, example or third-party).
- **Platform guest**: Bundled in the installer: `sqlite` and `local` only (`plugins/platform/`).
- **Optional plugin**: Bookclerk-maintained optional guests (storefronts, ABS, s3, d1, postgres under `plugins/optional/`).
- **Product plugin**: Another name for optional first-party guests (packaged with `cargo package-plugins`).
- **Reference example**: Echo samples under `examples/`. For CI and `cargo dev --examples` only, never packaged.
- **Third-party plugin**: Outside this monorepo, with the same jail and Workers RPC ABI.
- **Plugin package**: A Rust crate under `crates/bookclerk-plugins/`, or a workerd archive (`plugin.toml` + `modules/`).
- **In-process fallback**: None. Storefronts, destinations and database adapters run as staged external guests, and physical lowering (`openSession`, `dropUnit`, engine connect) stays in the adapter.
- **Verified artifact**: Installed bytes that match the install receipt (`manifest_sha256` + `payload_root_sha256`) for a provenance-qualified PluginKey.
- **`BookclerkEntrypoint`**: The workerd default-export base (`event(batch)` / `job(job)` / `databaseMigrations(binding)` / optional `describe()` / `shutdown()`), which extends `WorkerEntrypoint` in TypeScript. Named entrypoints are exported `*Entrypoint` subclasses, and Rust guests implement `PluginWorker` (`describe` / `open(invocation, bindings) -> Entrypoints`) + `serve` instead.

## Plugin aliases

A plugin's **alias** is its `plugin.toml` `id`. Aliases are for humans and
CLI ergonomics (`bookclerk plugins info postgres`). Durable ownership
(grants, sessions, databases, jobs, state directories, the install ledger)
uses the PluginKey.

### Unique on one host

Aliases are unique within **one host plugin namespace**
(`$BOOKCLERK_FILES_DIR`), across kinds (source, integration, output,
database). Each alias maps to zero or one installed `PluginKey`.

- Duplicate aliases from different PluginKeys on the **same host** are
  invalid installation state. Discovery fails closed, and install rejects
  the second package, including with `--replace`.
- Separate Bookclerk hosts with their own files directories may install
  the same alias and PluginKey independently, even when they share a
  library database. Aliases are **not** globally unique across an HA
  cluster.

### The grammar

The grammar is strict and non-lossy: lowercase `[a-z0-9_]{2,32}`, with no
leading or trailing `_` and no `__` (the same rule as the crates.io `{id}`
segment). Invalid characters are rejected at manifest load and install,
never rewritten, so values such as `a/b` and `a_b` can't collide after
sanitization.

### An alias doesn't change

An installed PluginKey's alias can't change across an ordinary install,
update or replace. A package can't keep the same PluginKey while changing
`id = "foo"` to `id = "foo2"`, because accounts keep `source` as the
display alias and CLI targeting resolves it.

> [!WARNING]
> An alias is **not** trust. A third-party package that is the only
> installed `sqlite` still receives no platform secrets, no SQLite library
> path and no first-party jail allowances.

## Develop locally

`cargo dev` builds a **lean** platform set (daemon, helpers and platform
guests), installs the platform guests into `$BOOKCLERK_FILES_DIR/plugins/`,
and runs `bookclerkd`. It restarts rather than reloading in place.
Optional storefronts are opt-in:

```sh
export BOOKCLERK_FILES_DIR=./BookclerkFiles   # optional; cargo-dev default
cargo dev                                     # lean platform + daemon
cargo dev --optional                          # + optional storefronts
cargo dev --examples                          # + reference Echo examples
cargo dev-cli -- version                      # lean CLI path
```

Finer-grained aliases use the same dispatcher (see
`crates/bookclerk-dev/README.md`):

```sh
cargo build-app --platform              # full default-members + platform guests
cargo build-app --optional --examples   # optional + Cargo examples
cargo install-platform                  # sqlite + local → FILES_DIR/plugins
cargo stage-plugins --optional          # optional → target/plugin-artifacts
cargo stage-plugins --examples --skip-build
cargo test-staged                       # describe/health conformance smoke (bookclerk-plugin-e2e)
```

- Add `--release` to any alias for release builds.
- Override the staging folder with `BOOKCLERK_PLUGIN_ARTIFACTS`.
- Pass host arguments after `--` (for example `cargo dev -- --help`).

First-party storefronts are always staged external guests. There is no
in-process host feature.

### The staged test suite

The staged suite lives in `crates/bookclerk-plugin-e2e`, so ordinary
`bookclerk-plugin-host` tests never need a staged installation.

- Set `BOOKCLERK_STAGED_PLUGINS=libro,echo_workerd_ts` to check a subset.
  Platform guests are always checked.
- Set `BOOKCLERK_REQUIRE_STAGED_PLUGINS=1` to fail, instead of skipping,
  when staging roots are missing.

## Reference examples

The Echo examples each have their own plugin id:

| Path | Runtime |
| --- | --- |
| [`examples/plugins-echo-native-rust`](../../examples/plugins-echo-native-rust) | native Rust (`api_version = 3`) |
| [`examples/plugins-echo-native-node`](../../examples/plugins-echo-native-node) | workerd JS (id `echo_native_node`) |
| [`examples/plugins-echo-native-python`](../../examples/plugins-echo-native-python) | workerd Python (id `echo_native_python`) |
| [`examples/plugins-echo-workerd-ts`](../../examples/plugins-echo-workerd-ts) | workerd TypeScript |
| [`examples/plugins-echo-workerd-python`](../../examples/plugins-echo-workerd-python) | workerd Python Workers |
| [`examples/plugins-echo-workerd-rust`](../../examples/plugins-echo-workerd-rust) | workerd Rust/Wasm |
| [`examples/plugins-echo-workerd-fetch`](../../examples/plugins-echo-workerd-fetch) | workerd outbound `*.example.com` + logo |

To build the TypeScript example:

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-ts && npm ci && npm run typecheck
```

### Try the Echo example

1. Stage the native Echo into the host plugin namespace. Don't copy it
   into a legacy `plugins/echo/` alias folder.

   ```sh
   cargo stage-plugins --examples
   ```

   The installer places the guest at `plugins/<PluginKey fs-id>/` (`pk-*`)
   and records host-owned ledger identity. Aliases such as `echo` stay
   presentation handles for the CLI and config.

   For a workerd Echo, install `plugin.toml` and `modules/` from any of
   [`plugins-echo-workerd-ts/`](../../examples/plugins-echo-workerd-ts),
   [`plugins-echo-workerd-python/`](../../examples/plugins-echo-workerd-python),
   [`plugins-echo-workerd-rust/`](../../examples/plugins-echo-workerd-rust)
   or
   [`plugins-echo-workerd-fetch/`](../../examples/plugins-echo-workerd-fetch)
   (outbound `*.example.com` and an embedded logo). The host spawns
   `bookclerk-workerd`, not a SEA binary.

2. Approve, deploy and configure it, then call it:

   ```console
   $ bookclerk plugins approve echo --yes
   $ bookclerk plugins list
   $ bookclerk plugins deploy echo          # deployment desired = present on this host
   $ bookclerk plugins config set echo greeting hello
   $ bookclerk integrations status
   echo enabled=true ok=true echo plugin ready
   $ bookclerk plugins echo ping --message hello
   pong: hello
   ```

## Distribution

What you ship depends on the runtime:

- **Native:** a folder (or archive) with `plugin.toml` and the binary for
  the target OS and architecture.
- **Workerd or script:** `plugin.toml` and `modules/`, with no per-OS
  binary. The operator's Bookclerk install already includes
  `bookclerk-workerd`, the pinned Cloudflare `workerd` binary and
  `bookclerk-jail`.

Operators then:

1. [Install](../server/plugins.md#installing-plugins) the package, with
   `bookclerk plugins install`, `POST /api/config/plugins`, or a package
   folder under `plugin-packages/`.
2. **Approve** its capabilities.
3. Deploy it with `bookclerk plugins deploy`.

A development tree unpacked under `plugins/` (or a
`BOOKCLERK_PLUGIN_DIRS` root) still runs as a host-local plugin. No
rebuild of Bookclerk is needed while `api_version` matches.

For **crates.io naming**, release-asset conventions, and installing
without Rust (the planned `bookclerk plugins search|install` and a
dashboard browser), see [Plugin registry](registry.md).

### First-party plugins

Audible, Libro.fm, Chirp, GraphicAudio and Audiobookshelf ship as
**external plugins** under `crates/bookclerk-plugins/`.

- `bookclerk-plugin-host` loads them through `load_sources` and
  `load_integrations` as staged guests, the same path third-party plugins
  use. The CLI and daemon call only those host helpers, never store crates
  by name.
- Discovery skips a PluginKey that is already registered. A clashing
  display alias from a different PluginKey on the same host is invalid
  (fail closed).

CI builds and stages plugin artifacts with `cargo stage-plugins`, as local
development does. Artifacts are **not** published to crates.io or GitHub
Releases yet.

```sh
cargo stage-plugins                    # build + stage to target/plugin-artifacts
cargo dev                              # stage + run bookclerkd
BOOKCLERK_PLUGIN_ARTIFACTS=/tmp/bc-plugins cargo stage-plugins   # another staging folder
```

## Next steps

- [plugin.toml](manifest.md): start your plugin's manifest.
- [Capabilities and consent](capabilities.md): ask for what your plugin
  needs.
- [Entrypoints, events and the ABI](abi.md): the methods your plugin
  implements.
- [Stores reference](../server/stores.md) and
  [Integrations](../server/integrations.md): the first-party plugins.
