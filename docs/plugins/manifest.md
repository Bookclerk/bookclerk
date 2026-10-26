# plugin.toml

The manifest every plugin ships: package layout, identity, runtime, entrypoints, triggers and the `[[settings]]` the operator fills in.

## Two files, two jobs

| File | Role |
| --- | --- |
| `plugin.toml` (next to the binary or `modules/`) | **Install / discovery** — id, runtime, command or `[workerd]`, capabilities / entrypoints |
| Plugin instance document (database) | **Operator settings** — values for the manifest's `[[settings]]`, sealed secrets, and a deployment per host (`present` = enabled) |

The plugin (or its installer) drops a directory under a search root. Bookclerk
scans for `plugin.toml`, enrols it (pins the schema, creates the default
instance), and when this host's deployment is present spawns
`bookclerk-workerd` (fronting the native `command` or loading the isolate)
with the instance document as `CONFIG`. Nothing about a plugin is written to
`config.toml`.

## Layout

Install trees are immutable and key-derived (`plugins/<PluginKey fs-id>/`, a
`pk-*` leaf). Mutable guest state is **not** nested in the install directory.

Native:

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

Workerd (script archive — no per-OS binary required):

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

`plugin-state/.../data` and `tmp` are created by the host at spawn, so a plugin
archive should not ship them. Deleting one plugin's state means deleting that
PluginKey's `plugin-state` tree. The install directory itself stays read-only.

Additional roots: `BOOKCLERK_PLUGIN_DIRS` (OS path list). A guest staged under one
of those still keeps its state under the files dir, so an upgrade that replaces
the staging tree does not take the plugin's state with it.

### `plugin.toml`

Native Echo:

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

Workerd Echo (TypeScript / Python / Rust-Wasm under
[`examples/plugins-echo-workerd-*`](../../examples); TOML shape matches TS):

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

There is **no** `protocol` or `kind` key. `api_version` must be `3`; the
handler family shown in the UI is derived from `entrypoints`, `[[events.consumers]]`,
and `[triggers]`. Optional top-level
`logo` sets the Settings favicon: an `https://` / `http://` URL (browser loads
it directly) or a relative image path under the plugin install root (host serves
`GET /api/plugins/{kind}/{id}/logo`). Embedded `.svg` logos are kept, but the
daemon always runs them through Cloudflare [`svg-hush`](https://crates.io/crates/svg-hush)
before serving and attaches a restrictive `Content-Security-Policy`
(`default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; script-src 'none'; object-src 'none'; sandbox`)
plus `X-Content-Type-Options: nosniff`; filter failure returns an error and never
the raw SVG. `img-src` allows sanitized embedded rasters; scripts stay blocked. Raster logos (`png` / `webp` /
`jpeg` / …) are unchanged. Do **not** invent `capabilities.network.domains`
on native plugins just for icons — domains remain **workerd-only** allowlists.

This is separate from describe-metadata **`BrandDto.icon_url`**: that field is the live
portal/Accounts brand returned by the guest after spawn (includes optional brand
colors `bg` / `fg` / `accent` plus `icon_url`). Optional `plugin.toml` `logo` is
install metadata so Settings can show an icon **without** spawning. When a
source/integration is loaded, Settings prefers the live brand `icon_url` over
`plugin.toml` `logo`. Keep them aligned when you ship both.

`command` (native) may be absolute or relative to the manifest directory. An
absolute `command` is granted read access on its own, so a manifest may point at
a binary installed elsewhere. For `runtime = "workerd"`, the host resolves
`bookclerk-workerd` beside itself; that launcher requires the pinned Cloudflare
`workerd` binary (`cargo ensure-workerd` / platform package) and loads
`[workerd]` + `modules/` into a real isolate. `compatibility_date` must be a
real calendar `YYYY-MM-DD`. workerd turns that date into runtime behavior: each
compatibility flag whose default-on date is on or before it is enabled, and the
binary refuses to start when the date is newer than the
`supported-compatibility-date` baked into that build. Wrangler does not pass
that newer date through. It warns and starts at the newest date the installed
runtime supports. Bookclerk does the same with this release's pin
(`2026-08-01`): `check` and load warn, then the isolate runs at the pin. An
equal or older date is passed through unchanged. The pin itself is
bumped on a **7-day publish cooldown** by CI — see
[packaging.md](../contributing/packaging.md#cloudflare-workerd-pin). The isolate runs at that
applied date. Host surfaces such as events and jobs do not yet choose behavior
from it. When they do, a behavior change ships with an enable date in a
Bookclerk release, and a plugin gets the behavior of the date this release
actually applied — not a second calendar, and not the raw author date when it
is past the pin. `[[kv_namespaces]]` is a
durable KV binding on the host library (see [KV namespaces](abi.md#kv-namespaces)).
`[[databases]]` does not replace it. `[queues]` is
typed: `[[queues.producers]]` rows are `env` send bindings and
`[[queues.consumers]]` rows are `queue(batch)` triggers, both on the host
outbox (see [Queues](abi.md#queues)). It is not a substitute for
`[[events.consumers]]` or `[[events.producers]]`, which are the bindings of
the host's own `core.events` queue.

Unknown capability keys are parse errors rather than silent defaults — a typo in
a security-relevant field must not read as "whatever we would have picked".

For plugins discovered from host-local plugin directories, two plugins that
claim the same `id` for the same `kind` are a **hard startup error** (CLI/daemon
exit); the same id across different kinds (e.g. a source and an integration
both named `echo`) is allowed there. An external id that collides with a
first-party plugin of the same kind is also rejected. Installed releases are
stricter: the id is the release's alias, and a version may not take an alias
that another plugin's active version already answers to, whatever its kind
(see [Installing plugins](../server/plugins.md#installing-plugins)).

### `[[settings]]`

Operator settings are declared in the manifest. The host renders Settings,
validates writes, and seeds defaults from this list; it hard-codes nothing
per plugin.

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

Non-secret entries need `[vars]` (that is how `CONFIG` reaches the guest);
`secret` entries need `[secrets]`. `required = true` makes a spawn fail with
an `error` observation when neither the document nor a default supplies the
key. The guest sees the schema defaults with the instance document on top
as `env.CONFIG`, and sealed values as `env.SECRETS`.

`describe().configOptions` is derived from this list: the SDK fills it in,
and a guest that returns its own non-empty list must return the same keys,
labels, and values or the spawn is refused. The schema is pinned in the
database at enrollment (`plugin_schemas`) together with the manifest's
semver `version`, so writes validate on any host. `version` is required: a
manifest without one is not enrolled, and the author CLI's `check`
([author tools](../contributing/packaging.md#plugin-author-tools-check--fmt--sync-embed--package--smoke)) rejects
it. A newer version re-pins the schema (by compare-and-swap, so two hosts
upgrading at once cannot leave the older version on top); every enrollment
pass then reconciles the plugin's instance documents against the pinned
schema, dropping values for keys it no longer declares and values that no
longer fit their declared type. A host whose installed package is older than
the pin does not enrol or spawn that plugin and records an `error`
observation naming the pinned version and the host that pinned it. A plugin
that declares no non-secret settings needs no `[vars]` binding and spawns
with an empty `CONFIG`.
