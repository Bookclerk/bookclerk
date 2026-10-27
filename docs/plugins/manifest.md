# plugin.toml

This page describes the manifest every plugin ships: the package layout,
identity, runtime, entrypoints, triggers, and the `[[settings]]` the
operator fills in.

## Two files, two jobs

| File | Role |
| --- | --- |
| `plugin.toml` (next to the binary or `modules/`) | **Install and discovery**: id, runtime, command or `[workerd]`, capabilities and entrypoints |
| The plugin instance document (in the database) | **Operator settings**: values for the manifest's `[[settings]]`, sealed secrets, and a deployment per host (`present` = enabled) |

How a plugin gets from disk to running:

1. The plugin (or its installer) drops a folder under a search root.
2. Bookclerk scans for `plugin.toml` and enrolls it: it pins the schema
   and creates the default instance.
3. When this host's deployment is present, Bookclerk spawns
   `bookclerk-workerd`, which fronts the native `command` or loads the
   isolate, with the instance document as `CONFIG`.

Nothing about a plugin is written to `config.toml`.

## Layout

Install trees are immutable and named from the key
(`plugins/<PluginKey fs-id>/`, a `pk-*` leaf). Mutable guest state is
**not** nested in the install folder.

A native plugin:

```text
$BOOKCLERK_FILES_DIR/
  plugins/pk-<fs-id>/
    plugin.toml
    bookclerk-plugin-echo-native-rust   # executable
  plugin-state/pk-<fs-id>/
    data/                               # host-created: guest state, its HOME
    tmp/                                # host-created: guest scratch, its TMPDIR
    session-<nonce>/                    # host-created: gateway only; removed on drop
```

A workerd plugin (a script archive, with no per-OS binary):

```text
$BOOKCLERK_FILES_DIR/
  plugins/pk-<fs-id>/
    plugin.toml
    modules/
      index.js
  plugin-state/pk-<fs-id>/
    data/
    tmp/
```

- The host creates `plugin-state/.../data` and `tmp` at spawn, so a plugin
  archive shouldn't ship them.
- Deleting one plugin's state means deleting that PluginKey's
  `plugin-state` tree. The install folder itself stays read-only.
- `BOOKCLERK_PLUGIN_DIRS` (an OS path list) adds more roots. A guest
  staged under one of those still keeps its state under the files folder,
  so an upgrade that replaces the staging tree doesn't take the plugin's
  state with it.

## Examples

A native Echo:

```toml
api_version = 3
id = "echo"
name = "Echo Integration"
version = "0.1.0"               # required semver; pins the [[settings]] schema
# logo = "https://example.com/icon.png"   # or "assets/logo.png" (host-served)
runtime = "native"
command = "./bookclerk-plugin-echo-native-rust"
entrypoints = ["cli"]           # storefront | storage | remoteLibrary | databaseAdapter | cli | oidc

[capabilities.network]
mode = "deny"

[vars]                          # `env.CONFIG`: [[settings]] defaults under the instance document

[[settings]]                    # operator settings schema (see below)
key = "greeting"
label = "Greeting"
type = "string"
default = "hi"

[[databases]]                   # plugin-owned isolated database → `env.DB`
binding = "DB"

[[events.consumers]]            # `event(batch)` trigger on the default export
type = "book_acquired"
schema_versions = [1]
supports_suspend = false

# Optional: CLI help without spawning (describe / cliDescribe win at invoke)
[cli]
[[cli.commands]]
name = "ping"
about = "Probe echo plugin"
[[cli.commands.args]]
name = "message"
long = "message"
kind = "string"
default = "hi"

# Optional: Bookclerk-as-IdP client templates without spawning
# (`oidcClients` RPC wins when the guest is loaded)
[[oidc.clients]]
client_id = "my-player"
display_name = "My Player"
callback_path = "/auth/openid/callback"
public_client = true
default_scopes = ["openid", "profile"]
issue_refresh_token = true
origin_config_key = "integrations.echo.base_url"
```

A workerd Echo (TypeScript, Python or Rust/Wasm under
[`examples/plugins-echo-workerd-*`](../../examples); the TOML shape
matches TypeScript):

```toml
api_version = 3
id = "echo"
name = "Echo Integration"
version = "1.0.0"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"
entrypoint = "default"

[workerd.limits]
cpu_ms = 30000
subrequests = 50

[capabilities.network]
mode = "deny"

[vars]

[[kv_namespaces]]
binding = "KV"

[[databases]]
binding = "DB"

[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
supports_suspend = true
```

## Identity and runtime

- There is **no** `protocol` or `kind` key. `api_version` must be `3`.
- The handler family shown in the UI is derived from `entrypoints`,
  `[[events.consumers]]` and `[triggers]`.
- Unknown capability keys are parse errors, not silent defaults. A typo in
  a security-relevant field must not read as "whatever we would have
  picked".

### `command` (native)

`command` may be absolute, or relative to the manifest folder. An absolute
`command` is granted read access on its own, so a manifest may point at a
binary installed elsewhere.

### `[workerd]`

For `runtime = "workerd"`, the host finds `bookclerk-workerd` beside
itself. That launcher needs the pinned Cloudflare `workerd` binary
(`cargo ensure-workerd`, or the platform package), and loads `[workerd]`
and `modules/` into a real isolate.

`compatibility_date` must be a real calendar date, `YYYY-MM-DD`. workerd
turns that date into runtime behavior:

- Each compatibility flag whose default-on date is on or before it is
  enabled.
- The binary refuses to start when the date is newer than the
  `supported-compatibility-date` baked into that build.

Wrangler doesn't pass a newer date through: it warns, and starts at the
newest date the installed runtime supports. Bookclerk does the same with
this release's pin (`2026-08-01`): `check` and load warn, then the isolate
runs at the pin. An equal or older date is passed through unchanged.

CI bumps the pin on a **7-day publish cooldown** (see
[Packaging → Cloudflare workerd pin](../contributing/packaging.md#cloudflare-workerd-pin)).
The isolate runs at that applied date.

> [!NOTE]
> Host surfaces such as events and jobs don't yet choose behavior from the
> date. When they do, a behavior change ships with an enable date in a
> Bookclerk release, and a plugin gets the behavior of the date this
> release actually applied: not a second calendar, and not the raw author
> date when it is past the pin.

### Bindings

- `[[kv_namespaces]]` is a durable KV binding on the host library (see
  [KV namespaces](abi.md#kv-namespaces)). `[[databases]]` doesn't replace
  it.
- `[queues]` is typed. `[[queues.producers]]` rows are `env` send
  bindings, and `[[queues.consumers]]` rows are `queue(batch)` triggers,
  both on the host outbox (see [Queues](abi.md#queues)).
- Queues aren't a substitute for `[[events.consumers]]` or
  `[[events.producers]]`, which are the bindings of the host's own
  `core.events` queue.

## Logo

An optional top-level `logo` sets the Settings favicon. It is either:

- an `https://` or `http://` URL, which the browser loads directly; or
- a relative image path under the plugin install root, which the host
  serves at `GET /api/plugins/{kind}/{id}/logo`.

Raster logos (`png`, `webp`, `jpeg`, …) are served unchanged. Embedded
`.svg` logos are kept, but the daemon always runs them through Cloudflare
[`svg-hush`](https://crates.io/crates/svg-hush) before serving them, and
adds:

- a restrictive `Content-Security-Policy`:
  `default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; script-src 'none'; object-src 'none'; sandbox`
  (`img-src` allows sanitized embedded rasters; scripts stay blocked);
- `X-Content-Type-Options: nosniff`.

A filter failure returns an error, never the raw SVG.

> [!NOTE]
> Don't add `capabilities.network.domains` to a native plugin just for an
> icon. Domains are **workerd-only** allowlists.

The logo is separate from the describe metadata **`BrandDto.icon_url`**.
That field is the live portal and Accounts brand the guest returns after
spawn, with optional brand colors `bg`, `fg` and `accent` plus `icon_url`.
The `plugin.toml` `logo` is install metadata, so Settings can show an icon
**without** spawning.

When a source or integration is loaded, Settings prefers the live brand
`icon_url` over the `plugin.toml` `logo`. Keep them aligned when you ship
both.

## Duplicate ids

For plugins discovered from host-local plugin folders:

- Two plugins that claim the same `id` for the same `kind` are a **hard
  startup error** (the CLI or daemon exits).
- The same id across different kinds (such as a source and an integration
  both named `echo`) is allowed there.
- An external id that clashes with a first-party plugin of the same kind
  is also rejected.

Installed versions are stricter. The id is the version's alias, and a
version may not take an alias that another plugin's active version already
answers to, whatever its kind (see
[Installing plugins](../server/plugins.md#installing-plugins)).

## `[[settings]]`

Operator settings are declared in the manifest. The host renders
Settings, checks writes, and seeds defaults from this list. It hard-codes
nothing per plugin.

```toml
[[settings]]
key = "bitrate"              # ^[a-z][a-z0-9_]{0,63}$, unique within the plugin
label = "Bitrate"
type = "enum"                # string | bool | integer | enum | secret
values = [{ id = "high", label = "High" }, { id = "normal", label = "Normal" }]
default = "high"
required = false
description = "Audible download quality."

[[settings]]
key = "api_key"
label = "API key"
type = "secret"              # sealed in the database, delivered in SECRETS; no default
```

- Non-secret entries need `[vars]`, which is how `CONFIG` reaches the
  guest. `secret` entries need `[secrets]`. A plugin that declares no
  non-secret settings needs no `[vars]` binding, and spawns with an empty
  `CONFIG`.
- The guest sees the schema defaults, with the instance document on top,
  as `env.CONFIG`, and sealed values as `env.SECRETS`.
- `required = true` makes a spawn fail with an `error` observation when
  neither the document nor a default supplies the key.

`describe().configOptions` is derived from this list. The SDK fills it
in, and a guest that returns its own non-empty list must return the same
keys, labels and values, or the spawn is refused.

### Versions pin the schema

The schema is pinned in the database at enrollment (`plugin_schemas`),
together with the manifest's semver `version`, so writes are checked on
any host.

- `version` is required. A manifest without one isn't enrolled, and the
  author CLI's `check`
  ([author tools](../contributing/packaging.md#plugin-author-tools-check--fmt--sync-embed--package--smoke))
  rejects it.
- A newer version pins the schema again, by compare-and-swap, so two hosts
  upgrading at once can't leave the older version on top.
- Every enrollment pass then reconciles the plugin's instance documents
  against the pinned schema. It drops values for keys the schema no
  longer declares, and values that no longer fit their declared type.
- A host whose installed package is older than the pin doesn't enroll or
  spawn that plugin. It records an `error` observation naming the pinned
  version and the host that pinned it.

## Next steps

- [Capabilities and consent](capabilities.md): the `[capabilities]`
  section in detail.
- [Entrypoints, events and the ABI](abi.md): what each entrypoint and
  trigger implements.
- [Managing plugins](../server/plugins.md#enabling-and-settings-database):
  how operators set these settings.
