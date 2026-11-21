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
entrypoints = ["cli"]           # storefront | storage | remoteLibrary | databaseAdapter | cli | oidc | metadata | covers | metadataFiles

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
- `[capabilities.network]` takes `mode`, `domains` (workerd), `tcp`,
  `address_cidrs`, `allow_undeclared_public_redirects` and an optional
  `[capabilities.network.own_user_agent]` table for hosts that need the
  plugin's own `User-Agent`. See
  [Capabilities and consent](capabilities.md#declaring-what-a-plugin-needs).

### `credentials` (storefronts)

A storefront says whether its store sign-in may be refreshed in more than
one place. A person's store connections follow them to the branches they
join either way; the declaration decides who refreshes them.

```toml
entrypoints = ["storefront"]
credentials = "shareable"   # or "single_keeper" (the default)
```

- `shareable`: the store's token is not refreshed or rotated, or several
  copies may refresh it on their own. Libro.fm, Chirp and GraphicAudio
  declare it.
- `single_keeper` (the default, also when the key is left out): one place
  keeps and refreshes the sign-in, so a copy refreshing on its own would
  break it. Audible is a single keeper. A branch keeps such an account for
  each library it shares it with and pushes its newer sign-ins there (see
  [One refresh keeper](../server/branches.md#one-refresh-keeper)).

A plugin counts as shareable only when every copy of it the install knows
says `shareable`.

The key is refused on a plugin without the `storefront` entrypoint.

### `[media]` (storefronts and metadata providers)

A storefront or metadata provider says which kinds of work and which
edition carriers it works with. The table is required for those plugins
and has no default: one without it, or with an empty `works` or `lists`,
is refused at install and at spawn. Every other plugin must leave it out.

```toml
entrypoints = ["storefront"]

[media]
works = ["book", "podcast"]   # book | podcast
lists = ["audio"]             # audio | ebook | print
fetches = ["audio"]           # what fetchTitle can deliver
```

- `works`: the work kinds the plugin reports.
- `lists`: the carriers its scans, catalog search and metadata records
  report.
- `fetches`: the carriers it can download, a subset of `lists`.
  Storefronts only; a metadata provider leaves it out.

`describe()` returns the same declaration, and the host refuses records
outside it. See [Declaring media](abi.md#declaring-media) for the rules.

### `[acquire]` (download pace)

A storefront can say how fast Bookclerk should download from one store
account. The table is optional, and only a plugin with the `storefront`
entrypoint may carry it.

```toml
[acquire]
downloads_per_day = 100   # 1 to 10000, over a rolling 24 hours
min_gap_seconds = 30      # 0 to 86400, between two downloads
cooldown_seconds = 86400  # 1 to 604800, pause after the store says slow down
```

- `downloads_per_day`: how many downloads Bookclerk starts on its own for
  one account in any 24 hours. Leave it out for no daily limit.
- `min_gap_seconds`: the shortest time between two downloads Bookclerk
  starts on its own. Leave it out for no gap.
- `cooldown_seconds`: how long the account rests after `fetchTitle` fails
  with `rate_limited` and no `retryAfterMs`. It defaults to one hour. When
  the store sends `retryAfterMs`, the account rests that long instead (at
  most a week).

The pace holds only downloads Bookclerk starts itself: auto-acquire and
requests from other libraries. A download a person starts is counted
toward the day but never held. The account's person (on **Accounts**) or
the operator (`bookclerk library set-pace`) can replace the declared pace
for one account with a custom pace or no limit. See
[Download pacing](../internals/jobs.md#download-pacing).

### `[metadata_files]` (metadata file dialects)

A plugin with the `metadataFiles` entrypoint lists the file formats it
reads, one `[[metadata_files.dialects]]` row each. The table is required
for that entrypoint, with at least one dialect, and every other plugin
must leave it out.

```toml
entrypoints = ["metadataFiles"]

[[metadata_files.dialects]]
id = "abs_metadata_json"
name = "Audiobookshelf metadata.json"
file_names = ["metadata.json"]
fields = ["title", "subtitle", "description", "publisher", "publishedAt", "language", "genres", "tags", "identifiers", "credits", "series", "chapters"]
credit_roles = ["author", "narrator"]
writes = true

[[metadata_files.dialects]]
id = "opf"
name = "OPF package"
suffixes = [".opf"]
fields = ["title", "description", "credits"]
credit_roles = ["author", "narrator"]
```

- `id`: `[a-z][a-z0-9_]{0,63}`, unique within the plugin. The host passes
  it back to `parse` and `render`.
- `name`: shown in settings and on a book's file list.
- `file_names` and `suffixes`: the files the dialect claims. Each is a
  plain ASCII file name; a suffix starts with `.`. Both compare without
  ASCII case, an exact name wins over a suffix and the longest suffix
  wins. A name or suffix may be claimed once per plugin, and a row needs
  at least one of them.
- `fields`: what the format carries, by wire name (`title`, `subtitle`,
  `description`, `publisher`, `publishedAt`, `language`, `genres`,
  `tags`, `abridgement`, `expressionKind`, `identifiers`, `credits`,
  `series`, `chapters`, `duration`). Never empty, no repeats.
- `credit_roles`: the roles the format carries. Required exactly when
  `fields` names `credits`.
- `writes`: whether `render` can write the format. Defaults to false, for
  read-only formats such as a store's own catalog JSON.

The host refuses a parsed record that states anything outside its row.
See [Metadata file dialects](abi.md#metadata-file-dialects) for the
methods and limits.

### `[notifier]` (notifiers)

A plugin with the `notifier` entrypoint says what it sends to. The table
is required for that entrypoint, and every other plugin must leave it out.

```toml
entrypoints = ["notifier"]

[notifier]
destinations = ["channel", "personal"]
account_mail = true
formats = ["text", "html"]

[[notifier.address_fields]]
key = "email"
label = "Email address"
required = true
```

- `destinations`: `channel` (one shared target the operator sets up in
  the instance's settings, such as a Slack webhook) and `personal` (each
  patron's own address). At least one, no repeats.
- `account_mail`: whether the plugin can carry account mail (invites,
  password resets, recovery notices). It needs a required, non-secret
  `email` address field. Defaults to false.
- `formats`: the bodies the service shows, from `text`, `markdown` and
  `html`. Must include `text`. Defaults to `["text"]`.
- `[[notifier.address_fields]]`: the fields of a personal address, at most
  16, required exactly when `destinations` names `personal`. `key` is
  `[a-z][a-z0-9_]{0,63}` and unique; `label` is shown to the patron;
  `required` and `secret` default to false. At least one field is
  required. A `secret` value (a personal token) is sealed at rest and never
  shown again.

See [Notifiers](abi.md#notifiers) for the methods and limits.

### `[[notifications.types]]` (notifications a plugin raises)

Any plugin may declare the notification types it raises through
`env.NOTIFY`. Declaring one grants the `NOTIFY` binding, so no other
binding may be named `NOTIFY`.

```toml
[[notifications.types]]
id = "sync_failed"
audience = "operational"
severity = "warning"
title = "Library sync failed"
```

- `id`: `[a-z][a-z0-9_]{0,63}`, unique within the plugin. The host names
  the type `<plugin id>:<id>` in routing rules.
- `audience`: `operational` (routed by the administrators to library
  channels) or `patron` (sent to the one patron a raise names, by that
  patron's preferences). `account` is reserved to Bookclerk.
- `severity`: `info`, `warning` or `critical`.
- `title`: one line of 1 to 256 bytes, shown in notification settings.

At most 64 types. See [Raising notifications](abi.md#raising-notifications)
for the binding.

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

- `[sockets]` asks for host-brokered WebSockets (`env.SOCKETS`, or the
  name in `binding`). It needs `[capabilities.network] mode =
  "outbound"`, and every URL must still pass the network grant (see
  [Pushed sockets](abi.md#pushed-sockets-sockets)).
- `[[webhooks]]` asks the host to take requests from outside services
  for each instance and keep them until the plugin pulls them
  (`env.WEBHOOKS`; see [Inbound webhooks](abi.md#inbound-webhooks-webhooks)).

### `[[webhooks]]` (inbound webhooks)

Each row is one webhook URL per plugin instance. A service such as a
storage provider's event notifications calls it; the host checks and
stores the request, and the plugin reads it through `env.WEBHOOKS`.

```toml
[[webhooks]]
name = "b2-events"                    # [a-z0-9][a-z0-9-]*, at most 32 chars
max_body_bytes = 65536                # default 64 KiB, at most 1 MiB
rate_per_minute = 60                  # default 60, at most 600
retention_secs = 86400                # default 1 day, 60 s to 7 days
headers = ["x-bz-event-notification-timestamp"]  # kept besides content-type
[webhooks.signature]
scheme = "hmac-sha256"                # or "none"
header = "x-bz-event-notification-signature"
prefix = "v1="                        # stripped before decoding, optional
encoding = "hex"                      # or "base64"; default hex
secret_setting = "webhook_secret"     # a type = "secret" [[settings]] key
```

- A plugin declares at most 8 rows, with unique names.
- `signature` is required, so an unsigned webhook is a choice:
  `signature = { scheme = "none" }`. An `hmac-sha256` rule names the
  header carrying the signature and the secret setting holding the key;
  until the operator sets that secret, the route answers 503.
- `headers` lists up to 16 lowercase header names the stored request
  keeps; `content-type` is always kept. Credential and hop-by-hop headers
  (`authorization`, `cookie`, `host`, `transfer-encoding` and the like)
  can't be kept.
- Declaring any row adds the `webhooks` binding token to the consent
  request.

### `[triggers]`

`[triggers]` declares handlers on the default export that the host calls
itself:

```toml
[triggers]
jobs = ["copy"]                                # job(controller) command types
crons = ["*/30 * * * *", "0 3 * * MON-FRI"]    # scheduled(controller), UTC
```

- `crons` holds at most 8 unique five-field cron schedules, read in UTC.
  `plugin check` checks their shape, and the host parses them fully at
  install; a schedule that can never fire (such as `0 0 30 2 *`) is
  rejected. The grammar and the delivery rules are in
  [Scheduled triggers](abi.md#scheduled-triggers).
- A plugin with `crons` cannot also declare the `storefront`, `storage` or
  `databaseAdapter` entrypoint, because schedules fire only on integration
  deployments. Put the schedule in a separate integration plugin.
- Each schedule is part of the consent request, as a `cron:<schedule>`
  token.
- `jobs` beside a `storefront`, `storage` or `databaseAdapter` entrypoint
  don't make the plugin an integration: the plugin stays in that
  entrypoint's handler family, and its jobs run on that session. The
  host's one job type today, `stream_copy`, runs this way on a storage
  plugin's destination. A plugin without one of those entrypoints that
  declares `jobs` is an integration.

## Logo

An optional top-level `logo` sets the Settings favicon. It is either:

- an `https://` or `http://` URL. The host downloads an `https://` logo
  through its cover service and serves a 128 px copy (see
  [Other images](../server/integrations.md#other-images)); the browser
  never loads it directly, and an `http://` logo isn't shown. A remote
  logo must be a JPEG, PNG or WebP image: an SVG or GIF can't be
  converted, and Settings shows a fallback icon instead; or
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
type = "enum"                # string | bool | integer | enum | secret | table
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
- `network = "fetch"` or `"tcp"` on a top-level `string` setting says its
  value names a server the plugin connects to. The value's destination
  appears on the consent form and is granted only once approved; see
  [Network settings](capabilities.md#network-settings). `default_port`
  (1-65535, only with `network`) is the port when the value names none.

### Table settings

A `table` setting holds a list of rows, such as one row per mapped
library. Its `[[settings.columns]]` declare the fields of each row. A
column is a `string`, `bool`, `integer` or `enum` entry with its own
`label`, `values`, `default`, `required` and `description`.

```toml
[[settings]]
key = "mappings"
label = "Library mappings"
type = "table"
default = [{ library = "main", prefix = "" }]

[[settings.columns]]
key = "library"
label = "Library"
type = "string"
required = true              # every row needs a value, from the row or the default

[[settings.columns]]
key = "prefix"
label = "Storage key prefix"
type = "string"

[[settings.columns]]
key = "watch"
type = "bool"
default = true               # a row that leaves the column out gets this value
```

- A table declares 1 to 16 columns and holds at most 64 rows.
- Rows are stored and delivered as objects keyed by column, so
  `env.CONFIG.mappings` is an array such as
  `[{ "library": "main", "prefix": "", "watch": true }]`. The TypeScript
  SDK types it from the columns.
- Adding a column keeps existing rows valid: a row that leaves a column
  out gets the column's default. A new column that is `required` needs a
  default for the same reason.
- When a newer version removes a column, its values are dropped from
  every row at enrollment, and the rest of each row is kept.

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
