# Dynamic plugins

Libation discovers **third-party plugins** at runtime and talks to them over
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

The plugin (or its installer) drops a directory under a search root. Libation
scans for `plugin.toml`, spawns `command`, and passes the matching main-config
table on `handshake`. Users never put `command` in `config.toml`.

## Layout

```text
$LIBATION_FILES_DIR/plugins/
  echo/
    plugin.toml
    libation-plugin-echo-integration   # executable
```

Additional roots: `LIBATION_PLUGIN_DIRS` (OS path list).

### `plugin.toml`

```toml
api_version = 1
id = "echo"
name = "Echo Integration"
kind = "integration"          # source | integration | output
command = "./libation-plugin-echo-integration"
# args = ["--verbose"]
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

Handshake params include `{ "api_version": 1, "config": {…} }` — the plugin’s
`[sources.<id>]` / `[integrations.<id>]` table from **main** `config.toml` as JSON
(empty object if the table is missing).

### Integration capabilities

Advertise in `handshake.capabilities`: `start`, `on_event`, `health`,
`diagnose`, `scan_library`, `authenticate_user`.

| Method | Notes |
| --- | --- |
| `start` | Background watchers |
| `on_event` | `{ "event": "book_liberated"\|"external_user_observed", "payload": … }` |
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
cargo build -p libation-plugin-echo-integration
mkdir -p "$LIBATION_FILES_DIR/plugins/echo"
cp target/debug/libation-plugin-echo-integration \
   "$LIBATION_FILES_DIR/plugins/echo/"
cat > "$LIBATION_FILES_DIR/plugins/echo/plugin.toml" <<'EOF'
api_version = 1
id = "echo"
kind = "integration"
command = "./libation-plugin-echo-integration"
EOF
```

```toml
# config.toml
[integrations.echo]
enabled = true
```

```bash
libation plugins list
libation integrations status
# echo enabled=true ok=true echo plugin ready
```

## Distribution

Ship a directory (or archive) containing `plugin.toml` + binary for the target
OS/arch. Users unpack under `plugins/` (or a `LIBATION_PLUGIN_DIRS` root) and set
`enabled = true` in `config.toml`. No rebuild of Libation is required when the
protocol version matches.
