# Dynamic plugins

Bookclerk discovers **third-party plugins** at runtime and talks to them over
newline-delimited [JSON-RPC 2.0](https://www.jsonrpc.org/specification) on
stdio. First-party sources/integrations stay in-process; external plugins are
separate executables that any language can implement.

## Why subprocesses?

Content sources and integrations need their own async runtimes, HTTPS clients,
and OAuth flows. Loading foreign `cdylib`s into the host process is fragile
across Rust/Tokio versions. A child process gives crash isolation, independent
releases, and a stable wire protocol.

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
kind = "integration"          # source | integration | output
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
`diagnose`, `scan_library`, `authenticate_user`, `cli`.

| Method | Notes |
| --- | --- |
| `start` | Background watchers |
| `on_event` | `{ "event": "book_acquired"\|"external_user_observed", "payload": … }` |
| `scan_library` | `{ "force": bool }` |
| `authenticate_user` | `{ "username", "password" }` → `ExternalUser` |

### Source capabilities

| Method | Notes |
| --- | --- |
| `login` / `list_accounts` | Auth under `files_dir` |
| `scan` | Receives `library_db` path; plugin opens SQLite |
| `fetch_title` | Write media under `cache_dir`; return `plain` paths |

Encrypted/DRM fetch is not in the v1 external protocol yet (first-party only).

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
