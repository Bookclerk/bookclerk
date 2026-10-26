# Dynamic plugins

Bookclerk is built around pluggable **sources**, **destinations**, and
**integrations**. First-party **guest** implementations live under
`crates/bookclerk-plugins/{platform,optional}/` (one package per plugin). The
**host** runtime is
[`bookclerk-plugin-host`](../../crates/bookclerk-plugin-host) — discovery, spawn,
jail, consent, and Workers RPC — distinct from the runtime install tree
`$BOOKCLERK_FILES_DIR/plugins/`.

The product ABI is **object-capability Workers RPC** at `api_version = 3`
(capability entrypoints + triggers, transferred `ReadableStream` / Cap'n
Proto byte sources, no public `handleId` / `writeChunk` / base64 media).
Native guests serve the Bookclerk Cap'n Proto schema on stdio; workerd
guests keep isolate `RpcTarget` stubs. Each HTTP/RPC request is
**invocation-scoped**: the trusted adapter opens the guest, invokes the
method, and disposes stubs before the request completes. Survival across
suspend is checkpoint data plus stable binding identifiers — never an
in-memory `RpcTarget`, adapter map id, PID, or open connection. Decision
records: [`docs/adr/plugin-capabilities-v3.md`](../adr/plugin-capabilities-v3.md)
(v3 surface) and
[`docs/adr/plugin-workers-rpc-workerd.md`](../adr/plugin-workers-rpc-workerd.md)
(jail / workerd front door). Authoritative artifacts: Cap'n Proto
[`schema/plugin.capnp`](../../crates/bookclerk-plugin-abi/schema/plugin.capnp)
(append-only ordinals; unknown union members fail closed or return typed
`unsupported`) and TypeScript
[`packages/plugin-sdk/src/plugin.ts`](../../packages/plugin-sdk/src/plugin.ts).

Authors write a Workers-shaped module. The default export extends
**`BookclerkEntrypoint`** and implements the triggers `plugin.toml` declares —
`event(batch)` for `[[events.consumers]]`, `job(job)` for `[triggers] jobs`,
`databaseMigrations(binding)` for `[[databases]]` — plus an optional
`describe()` refinement. Every named entrypoint in `entrypoints = [...]` is an
exported class with the matching name extending its base (`Storefront`,
`Storage`, `RemoteLibrary`, `DatabaseAdapter`, `Cli`, `Oidc`). Native guests
implement Rust `PluginWorker` (`describe` / `open(invocation, bindings) ->
Entrypoints`) and call `serve`. The trusted adapter isolate installs the
granted bindings on `env` per invocation (`CONFIG`, `SECRETS`, `EVENTS`,
`WORK_FS`, named databases) and never exposes HTTP endpoints, PIDs,
credentials, or Cap'n Proto to authors. Native guests behind workerd are
reached as typed Cap'n Proto by the launcher, never through an isolate
binding. Byte `Source` is the job input
opener; storefronts are the separately named `storefront` entrypoint. JSON is
allowed only for plugin-specific extensible config (`schemaVersion` +
`mediaType`/`schemaId` + bounded payload).

On native guests, `serve` is the stdin/stdout Cap'n Proto runner for
`api_version = 3`.

| Language | Package | Notes |
| --- | --- | --- |
| TypeScript | [`@bookclerk/plugin-sdk`](../../packages/plugin-sdk) | `/workerd` exports `BookclerkEntrypoint` + `*Entrypoint` bases; `bookclerk-plugin types` generates `Env` from `plugin.toml` |
| Python | [`bookclerk-plugin-sdk`](../../packages/plugin-sdk-python) | `from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, …`; module-level `Default` + named classes |
| Rust | [`bookclerk-plugin-sdk`](../../crates/bookclerk-plugin-sdk) | `PluginWorker` + `serve`; workerd guests pair `BookclerkEntrypoint` JS glue with Wasm dispatch |

Runtimes in `plugin.toml`:

| `runtime` | How it runs |
| --- | --- |
| `native` | OS binary speaking the same Workers RPC ABI. The host starts two sibling jails: `bookclerk-workerd` (control plane) and the native backend, joined by inherited duplex links |
| `workerd` | Author `modules/` loaded by first-party [`bookclerk-workerd`](../../crates/bookclerk-workerd) — **one jail + one isolate per plugin** (local embed, not Cloudflare cloud execution) |

For the product overview see the [documentation index](../README.md). Built-in
storefronts: [stores.md](../server/stores.md). Audiobookshelf / Connect:
[integrations.md](../server/integrations.md). Publishing / crates.io taxonomy and
standalone author repos: [plugin-registry.md](registry.md).

## Pages on this shelf

- [plugin.toml](manifest.md): package layout, identity, runtime, entrypoints
  and settings.
- [Capabilities and consent](capabilities.md): what a plugin may ask for, and
  how the operator approves it.
- [Entrypoints, events and the ABI](abi.md): the Workers RPC surface, events,
  KV, queues and every capability's methods.
- [Plugin registry](registry.md): crate naming, release assets and the
  planned registry.
- [SQL contract v1](../sql-contract/v1.md): what a database adapter must
  accept.

Operators install, approve and place plugins as described in
[Managing plugins](../server/plugins.md). How the host confines every guest
is in [The guest jail](../internals/guest-jail.md).

## Terminology

| Term | Meaning |
| --- | --- |
| **Plugin host** | Crate `bookclerk-plugin-host`; loads guests, never ships storefront logic by default |
| **External guest** | Any jailed subprocess plugin (platform, product, example, or third-party) |
| **Platform guest** | Bundled in the installer: `sqlite`, `local` only (`plugins/platform/`) |
| **Optional plugin** | Bookclerk-maintained optional guests (storefronts, ABS, s3, d1, postgres under `plugins/optional/`) |
| **Product plugin** | Synonym for optional first-party guests (packaged via `cargo package-plugins`) |
| **Reference example** | Echo samples under `examples/`; CI/`cargo dev --examples` only — never packaged |
| **Third-party plugin** | Outside this monorepo; same jail + Workers RPC ABI |
| **Plugin package** | Rust crate under `crates/bookclerk-plugins/`, or a workerd archive (`plugin.toml` + `modules/`) |
| **In-process fallback** | None. Storefronts, destinations, and database adapters run as staged external guests. Physical lowering (`openSession`, `dropUnit`, engine connect) stays in the adapter. |
| **Verified artifact** | Installed bytes that match the install receipt (`manifest_sha256` + `payload_root_sha256`) for a provenance-qualified PluginKey |
| **`BookclerkEntrypoint`** | Workerd default-export base (`event(batch)` / `job(job)` / `databaseMigrations(binding)` / optional `describe()` / `shutdown()`); named entrypoints are exported `*Entrypoint` subclasses. TS extends `WorkerEntrypoint`. Rust guests implement `PluginWorker` (`describe` / `open(invocation, bindings) -> Entrypoints`) + `serve` |

## Local development (external guests)

`cargo dev` builds a **lean** platform set (daemon + helpers + platform guests),
installs platform into `$BOOKCLERK_FILES_DIR/plugins/`, and runs `bookclerkd`
(restart-based; no HMR). Optional storefronts are opt-in:

```bash
export BOOKCLERK_FILES_DIR=./BookclerkFiles   # optional; cargo-dev default
cargo dev                                        # lean platform + daemon
cargo dev --optional                             # + optional storefronts
cargo dev --examples                             # + reference Echo examples
cargo dev-cli -- version                         # lean CLI path
```

Granular aliases (same dispatcher — see `crates/bookclerk-dev/README.md`):

```bash
cargo build-app --platform              # full default-members + platform guests
cargo build-app --optional --examples   # optional + Cargo examples
cargo install-platform                  # sqlite + local → FILES_DIR/plugins
cargo stage-plugins --optional          # optional → target/plugin-artifacts
cargo stage-plugins --examples --skip-build
cargo test-staged                       # describe/health conformance smoke (bookclerk-plugin-e2e)
```

The staged suite lives in `crates/bookclerk-plugin-e2e` so ordinary
`bookclerk-plugin-host` tests never need a staged installation. Set
`BOOKCLERK_STAGED_PLUGINS=libro,echo_workerd_ts` to check a subset (platform
guests are always checked) and `BOOKCLERK_REQUIRE_STAGED_PLUGINS=1` to fail
instead of skipping when staging roots are missing.

Add `--release` to any alias for release builds. Override staging dir with
`BOOKCLERK_PLUGIN_ARTIFACTS`. Forward host args after `--` (e.g. `cargo dev -- --help`).

First-party storefronts are always staged external guests — there is no
in-process host feature.

Reference Echo examples (distinct plugin ids):

Plugin **aliases** (`plugin.toml` `id`) are unique within **one host plugin
namespace** (`$BOOKCLERK_FILES_DIR`): each alias maps to exactly zero or one
installed [`PluginKey`]. Duplicate aliases from different PluginKeys on the
**same host** are invalid installation state (discovery fails closed; install
rejects the second package, including with `--replace`). Distinct Bookclerk
hosts with their own files directories may install the same alias and
PluginKey independently — even when they share a library database. Aliases
are **not** globally unique across an HA cluster.

An installed PluginKey's alias is immutable across ordinary
install/update/replace — a package cannot keep the same PluginKey while
changing `id = "foo"` to `id = "foo2"`, because accounts keep `source` as the
display alias and CLI targeting resolves it. Aliases are
for humans and CLI ergonomics (`bookclerk plugins info postgres`). They are
**not** trust: a third-party package that is the sole installed `sqlite`
still receives no platform secrets, SQLite library path, or first-party jail
allowances. Durable ownership (grants, sessions, databases, jobs, state
directories, the install ledger) uses PluginKey.

Plugin aliases are unique across kinds (source / integration / output /
database) **within one host plugin namespace**. The grammar is strict and
non-lossy: lowercase `[a-z0-9_]{2,32}` with no leading/trailing `_` and no
`__` (same rule as the crates.io `{id}` segment). Invalid characters are
rejected at manifest load / install — never rewritten — so values like `a/b`
and `a_b` cannot collide after sanitization.

| Path | Runtime |
| --- | --- |
| [`examples/plugins-echo-native-rust`](../../examples/plugins-echo-native-rust) | native Rust (`api_version = 3`) |
| [`examples/plugins-echo-native-node`](../../examples/plugins-echo-native-node) | workerd JS (id `echo_native_node`) |
| [`examples/plugins-echo-native-python`](../../examples/plugins-echo-native-python) | workerd Python (id `echo_native_python`) |
| [`examples/plugins-echo-workerd-ts`](../../examples/plugins-echo-workerd-ts) | workerd TypeScript |
| [`examples/plugins-echo-workerd-python`](../../examples/plugins-echo-workerd-python) | workerd Python Workers |
| [`examples/plugins-echo-workerd-rust`](../../examples/plugins-echo-workerd-rust) | workerd Rust/Wasm |
| [`examples/plugins-echo-workerd-fetch`](../../examples/plugins-echo-workerd-fetch) | workerd outbound `*.example.com` + logo |

```bash
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-ts && npm ci && npm run typecheck
```

## Examples

Native Echo (stage into the host plugin namespace; do not copy into a
legacy `plugins/echo/` alias directory):

```bash
cargo stage-plugins --examples
```

The installer places the guest at `plugins/<PluginKey fs-id>/` (`pk-*`) and
records host-owned ledger identity. Aliases such as `echo` remain presentation
handles for CLI/config.

Workerd Echo — install `plugin.toml` + `modules/` from any of
[`examples/plugins-echo-workerd-ts/`](../../examples/plugins-echo-workerd-ts),
[`plugins-echo-workerd-python/`](../../examples/plugins-echo-workerd-python),
[`plugins-echo-workerd-rust/`](../../examples/plugins-echo-workerd-rust), or
[`plugins-echo-workerd-fetch/`](../../examples/plugins-echo-workerd-fetch)
(outbound `*.example.com` + embedded logo; host spawns `bookclerk-workerd`,
not a SEA binary).

```bash
bookclerk plugins approve echo --yes
bookclerk plugins list
bookclerk plugins deploy echo          # deployment desired = present on this host
bookclerk plugins config set echo greeting hello
bookclerk integrations status
# echo enabled=true ok=true echo plugin ready
bookclerk plugins echo ping --message hello
# pong: hello
```

## Distribution

**Native:** ship a directory (or archive) containing `plugin.toml` + binary for
the target OS/arch.

**Workerd / script:** ship `plugin.toml` + `modules/` (no per-OS author binary).
The operator’s Bookclerk install already includes `bookclerk-workerd`, the pinned
Cloudflare `workerd` binary, and `bookclerk-jail`.

Operators [install](../server/plugins.md#installing-plugins) a package (`bookclerk plugins
install`, `POST /api/config/plugins`, or a package directory under
`plugin-packages/`), **approve** its capabilities, then deploy it with
`bookclerk plugins deploy`. A development tree unpacked under `plugins/` (or
a `BOOKCLERK_PLUGIN_DIRS` root) still runs as a host-local plugin. No
rebuild of Bookclerk is required when `api_version` matches.

### First-party plugins (staged guests)

Audible, Libro.fm, Chirp, GraphicAudio, and Audiobookshelf ship as **external
plugins** under `crates/bookclerk-plugins/`. The host crate
`bookclerk-plugin-host` loads them through `load_sources` /
`load_integrations` as staged guests — the same path third-party plugins
use. CLI/daemon call only those host helpers — never store crates by name.
Discovery skips a PluginKey that is already registered. A colliding display
alias from a different PluginKey on the same host is invalid (fail closed).

Guest binaries depend on **`bookclerk-plugin-sdk`** (+ their private store crate
for first-party). TypeScript workerd guests depend on **`@bookclerk/plugin-sdk`**.
Third-party authors should depend on the SDK only — not
`bookclerk-plugin-host`, `bookclerk-library`, or `bookclerk-source`.

CI builds and stages plugin artifacts with `cargo stage-plugins` (same as local
dev). Artifacts are **not** published to crates.io / GitHub Releases yet.

Locally:

```bash
cargo stage-plugins                    # build + stage to target/plugin-artifacts
cargo dev                       # stage + run bookclerkd
# or: BOOKCLERK_PLUGIN_ARTIFACTS=/tmp/bc-plugins cargo stage-plugins
```

For **crates.io naming**, release-asset conventions, and install-without-Rust
(planned `bookclerk plugins search|install` + dashboard browser), see
[plugin-registry.md](registry.md).
