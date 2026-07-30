# Dynamic plugins

Bookclerk is built around pluggable **sources**, **destinations**, and
**integrations**. First-party adapters ship in-process. This document covers
**third-party plugins**: separate executables discovered at runtime and talked
to over newline-delimited [JSON-RPC 2.0](https://www.jsonrpc.org/specification)
on stdio (any language).

For the product overview see the [documentation index](README.md). Built-in
storefronts: [sources.md](sources.md). Audiobookshelf / Connect:
[integrations.md](integrations.md).

## Why subprocesses?

Content sources and integrations need their own async runtimes, HTTPS clients,
and OAuth flows. Loading foreign `cdylib`s into the host process is fragile
across Rust/Tokio versions. A child process gives crash isolation, independent
releases, and a stable wire protocol.

## Trust model (external plugins are untrusted)

External plugins run as a **separate OS process** but are **not** a security
sandbox by themselves. Bookclerk hardens the host boundary:

| Host guarantees | Detail |
| --- | --- |
| No library DB path | `library.db` is never passed on the wire |
| No files-dir root | Plugins get `plugin_data_dir` (`…/plugins/<id>/data`) and fetch `cache_dir` only — not `master.key` |
| Env scrub | Child spawn uses `env_clear` + a small allowlist (`PATH`, `HOME`, locale, …). `BOOKCLERK_*`, `AWS_*`, tokens, and DB URLs are not inherited |
| Host-mediated secrets | `login` returns `{ account, credentials }`; host seals into `encrypted_secrets` with `provider = plugin id`. `scan` and `fetch_title` receive those blobs from the host |
| Host-mediated library writes | `scan` returns book DTOs; host upserts with `source` forced to the plugin id. `list_accounts` is answered from the host accounts table |
| Scoped identity | Plugin cannot claim another storefront’s `source` / `provider` |

First-party sources (Audible, Libro.fm, Chirp, GraphicAudio) ship **in-process**
for ease of development, but they use the same host-enforced `SourceScope`
boundary as third-party plugins: `source` / `provider` is forced to the plugin
id, and secrets for other plugins are invisible. Audible is not a separate
privilege class; its DRM pipeline is just richer (`Encrypted` fetch / licenses).
Client reuse for Audible lives in `open_account_client`, same idea as the library
unseal cache. Packaging may later omit first-party adapters from the binary;
scoping will remain identical.

Enabling a third-party plugin still means running that binary as the Bookclerk
user — review plugins before enabling them.

## Two files, two jobs

| File | Role |
| --- | --- |
| `plugin.toml` (next to the binary) | **Install / discovery** — id, kind, command, args |
| `config.toml` (`[sources.<id>]` / `[integrations.<id>]`) | **User settings** — `enabled`, opaque knobs |

The plugin (or its installer) drops a directory under a search root. Bookclerk
scans for `plugin.toml`, spawns `command`, and passes the matching main-config
table on `handshake`. Users never put `command` in `config.toml`.

## Layout

```text
$BOOKCLERK_FILES_DIR/plugins/
  echo/
    plugin.toml
    bookclerk-plugin-echo-integration   # executable
```

Additional roots: `BOOKCLERK_PLUGIN_DIRS` (OS path list).

### `plugin.toml`

```toml
api_version = 1
id = "echo"
name = "Echo Integration"
kind = "integration"          # source | integration | output | database
command = "./bookclerk-plugin-echo-integration"
# args = ["--verbose"]

# Optional: CLI help without spawning (handshake/cli.describe win at invoke)
[cli]
[[cli.commands]]
name = "ping"
about = "Probe echo plugin"
[[cli.commands.args]]
name = "message"
long = "message"
kind = "string"
default = "hi"
```

`command` may be absolute or relative to the manifest directory.

Two plugins that claim the same `id` for the same `kind` are a **hard startup
error** (CLI/daemon exit). The same id across different kinds (e.g. a source and
an integration both named `echo`) is allowed. An external id that collides with
a first-party plugin of the same kind is also rejected.

## Enabling and settings in `config.toml`

Plugin `id` must match a config table. **External integrations default to
disabled**; sources follow the usual `[sources.<id>]` rules (missing → enabled).

```toml
[integrations.echo]
enabled = true
# greeting = "hi"   # opaque knobs → handshake config

[sources.my_store]
enabled = true
# … opaque knobs …
```

## Protocol (api_version = 1)

Host → plugin: one JSON object per line on stdin.  
Plugin → host: one JSON-RPC response per line on stdout.  
Stderr is free for logging.

### Common

| Method | Purpose |
| --- | --- |
| `handshake` | Negotiate version, id, kind, capabilities, brand |
| `health` | Connectivity / config check |
| `diagnose` | Human-readable CLI probe lines |
| `cli.describe` | Declared CLI command schema (`CliSchema`) |
| `cli.invoke` | Run a declared command (`CliInvokeParams` → `CliInvokeResult`) |

Handshake params include `{ "api_version": 1, "config": {…} }` — the plugin’s
`[sources.<id>]` / `[integrations.<id>]` table from **main** `config.toml` as JSON
(empty object if the table is missing).

Optional handshake field `cli` may embed the same schema as `cli.describe`. Prefer
advertising capability `cli` and implementing both methods. You may also mirror
the schema in `plugin.toml` under `[cli]` so `bookclerk plugins <id> --help`
works without spawning the plugin; at invoke time handshake / `cli.describe`
remain authoritative.

### Plugin CLI

Host commands that apply to every plugin stay on Bookclerk verbs
(`plugins list|info|diagnose|enable|disable`, `integrations …`, `auth …`).
Plugin-specific commands are declared and invoked as:

```bash
bookclerk plugins <plugin-id> <command> [args…]
```

Example schema (JSON / handshake `cli` / `cli.describe`):

```json
{
  "commands": [
    {
      "name": "ping",
      "about": "Probe echo plugin",
      "args": [
        {
          "name": "message",
          "long": "message",
          "kind": "string",
          "required": false,
          "default": "hi"
        }
      ]
    }
  ]
}
```

`cli.invoke` params: `{ "command": "ping", "args": { "message": "hi" } }`.
Result: `{ "exit_code": 0, "stdout": "…", "stderr": "…", "json": … }`.

### Integration capabilities

Advertise in `handshake.capabilities`: `start`, `on_event`, `health`,
`diagnose`, `scan_library`, `sync_listening`, `authenticate_user`, `cli`.

| Method | Notes |
| --- | --- |
| `start` | Background watchers |
| `on_event` | `{ "event": "book_acquired"\|"external_user_observed", "payload": … }` |
| `scan_library` | `{ "force": bool }` |
| `sync_listening` | Return `{ "items": [ ListeningProgressSnapshot, … ] }`; host upserts tagged with plugin id |
| `authenticate_user` | `{ "username", "password" }` → `ExternalUser` |

### Source capabilities

| Method | Notes |
| --- | --- |
| `login` | Params: `plugin_data_dir`, marketplace/label/email/password. Result: `{ account, credentials? }` — host seals credentials (`provider = plugin id`) and upserts the account row |
| `list_accounts` | Host-only (accounts table for this plugin id); plugin is not called |
| `scan` | Params: `plugin_data_dir`, filters, and host-injected `credentials` map (`account_id` → opaque JSON; **no** `library_db`). Result includes `books[]` DTOs; host upserts with `source` forced to plugin id |
| `fetch_title` | Host injects `credentials` from `encrypted_secrets`; plugin writes media under `cache_dir` and returns `plain` paths |

Encrypted/DRM fetch is not in the v1 external protocol yet (first-party only).
Plugins must not open `library.db` or read `master.key`.

### Output plugins

`kind = "output"` is discovered and logged; loading is not implemented yet.

## Example

```bash
cargo build -p bookclerk-plugin-echo-integration
mkdir -p "$BOOKCLERK_FILES_DIR/plugins/echo"
cp target/debug/bookclerk-plugin-echo-integration \
   "$BOOKCLERK_FILES_DIR/plugins/echo/"
cat > "$BOOKCLERK_FILES_DIR/plugins/echo/plugin.toml" <<'EOF'
api_version = 1
id = "echo"
kind = "integration"
command = "./bookclerk-plugin-echo-integration"

[cli]
[[cli.commands]]
name = "ping"
about = "Probe echo plugin"
[[cli.commands.args]]
name = "message"
long = "message"
kind = "string"
default = "hi"
EOF
```

```toml
# config.toml
[integrations.echo]
enabled = true
```

```bash
bookclerk plugins list
bookclerk plugins enable echo
bookclerk integrations status
# echo enabled=true ok=true echo plugin ready
bookclerk plugins echo ping --message hello
# pong: hello
```

## Distribution

Ship a directory (or archive) containing `plugin.toml` + binary for the target
OS/arch. Users unpack under `plugins/` (or a `BOOKCLERK_PLUGIN_DIRS` root) and set
`enabled = true` in `config.toml`. No rebuild of Bookclerk is required when the
protocol version matches.
