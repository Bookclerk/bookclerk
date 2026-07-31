# Dynamic plugins

Bookclerk is built around pluggable **sources**, **destinations**, and
**integrations**. First-party adapters live under `crates/bookclerk-plugins/`
as a **single package per plugin** with dual targets: a library (`register()` for
an easy `cargo run`) and a JSON-RPC guest binary for distribution / staging.
This document covers the **external** (subprocess) model — also used by
third-party plugins — over newline-delimited [JSON-RPC 2.0](https://www.jsonrpc.org/specification)
on stdio (any language).

For the product overview see the [documentation index](README.md). Built-in
storefronts: [sources.md](sources.md). Audiobookshelf / Connect:
[integrations.md](integrations.md). Publishing / crates.io taxonomy and
standalone author repos: [plugin-registry.md](plugin-registry.md).

## Why subprocesses?

Content sources and integrations need their own async runtimes, HTTPS clients,
and OAuth flows. Loading foreign `cdylib`s into the host process is fragile
across Rust/Tokio versions. A child process gives crash isolation, independent
releases, and a stable wire protocol.

## Trust model (external plugins are untrusted)

External plugins run as a **separate OS process**, confined by the host to the
directories it hands them ([the guest jail](#the-guest-jail)). The protocol
boundary is narrow on top of that:

| Host guarantees | Detail |
| --- | --- |
| No library DB path | `library.db` is never passed on the wire — and not reachable if it were |
| No files-dir root | Plugins get `plugin_data_dir` (`…/plugins/<id>/data`) and fetch `cache_dir` only — not `master.key` |
| Env scrub | Child spawn uses `env_clear` + a small allowlist (`PATH`, locale, …). `BOOKCLERK_*`, `AWS_*`, tokens, and DB URLs are not inherited; `HOME` and `TMPDIR` are replaced with the guest's own directories |
| Host-mediated secrets | `login` returns `{ account, credentials }`; host seals into `encrypted_secrets` with `provider = plugin id`. `scan` and `fetch_title` receive those blobs from the host |
| Host-mediated library writes | `scan` returns book DTOs; host upserts with `source` forced to the plugin id. `list_accounts` is answered from the host accounts table |
| Scoped identity | Plugin cannot claim another storefront’s `source` / `provider` |

First-party sources and Audiobookshelf all ship under `crates/bookclerk-plugins/`
with the same guest SDK contract. The **plugin host** crate
(`bookclerk-plugin`) also calls `register_builtin_sources` /
`register_builtin_integrations` so in-process library crates work for
`cargo run` without staging binaries — host binaries never name store crates.
In-process source **and** Audiobookshelf crates are optional Cargo features
named after the plugin packages (`bookclerk-plugin-source-audible`,
`bookclerk-plugin-integration-audiobookshelf`, …), carried by the same names on
both hosts; `--no-default-features` builds an external-guest-only host (see
[Shipping without a store](#shipping-without-a-store)). Discovered external
copies of the same id are skipped. After registration, hosts talk **only**
through `ContentSource` /
`Integration` (login, scan, fetch, import, revoke, inspect, plus catalog
`search_catalog` / `expand_candidates` / `purchase_hint` / `list_deals` for
Discover). Sources always return `PlainFetch` (`SourceFetch` is an alias) —
DRM (Adrm/CENC) is decrypted inside the Audible plugin before the host sees
media. Guest `fetch_title` carries optional `pdf_url`; catalog methods are on
the JSON-RPC wire for external guests.

Enabling a third-party plugin still means running that binary as the Bookclerk
user, inside the jail below — review plugins before enabling them.

## Shipping without a store

Both hosts carry one feature per in-process plugin, all on by default, so a build
can leave any of them out:

```bash
# External guests only: no store linked into either host.
cargo build --release -p bookclerk-cli -p bookclerkd --no-default-features

# Everything except Audible.
cargo build --release -p bookclerk-cli -p bookclerkd --no-default-features \
  --features bookclerk-plugin-source-libro,bookclerk-plugin-source-chirp,bookclerk-plugin-source-graphicaudio,bookclerk-plugin-integration-audiobookshelf
```

This exists for Audible specifically. Adrm and Widevine CENC decrypt live in that
plugin, and some regions restrict distributing a binary that can circumvent
DRM — so whoever packages Bookclerk needs the option to ship hosts that contain
no such code at all, rather than a build flag that merely disables it at runtime.
Omitting the feature omits the crate, and with it the ciphers, the content-key
handling, and the CDM.

Nothing else has to move for that to hold. The shared MP4 plumbing
(`bookclerk-mp4`) parses and rewrites containers and takes a `SampleTransform`
from its caller; the Audible plugin's transform is the only one that decrypts.
`scripts/check-store-free-hosts.sh` asserts it in CI: a `--no-default-features`
host must link no plugin package and reach no cipher crate, and a default build
must still link Audible — otherwise the first check would pass for the wrong
reason. A shared crate that grew an `aes` dependency fails that job.

Users of a store-free build can still add any storefront back as an external
guest, since discovery is independent of these features. That is a deployment
choice made by whoever installs the plugin, not by whoever shipped the binary.

## The guest jail

Every external guest is started by **`bookclerk-jail`**, a small launcher that
applies a confinement policy to itself and then `exec`s the plugin. What it
grants is decided entirely by the host.

A guest gets four paths and nothing else:

| Path | Access | Also known to the guest as |
| --- | --- | --- |
| its install directory | read-only | `cwd` |
| `…/plugins/<id>/data` | read/write | `HOME`, and `plugin_data_dir` on the wire |
| `…/plugins/<id>/tmp` | read/write | `TMPDIR` / `TEMP` / `TMP` |
| one fetch work directory at a time | write (via descriptor) | passed on fd 3 immediately before each `fetch_title` |

Plus the system read paths every process needs to start (the loader, shared
libraries, the CA bundle, resolver config) and a writable `/dev/null`.

That leaves out `master.key`, `library.db`, `config.toml`, the operator token,
the finished library, the download cache root, and every *other* plugin's data
directory. None of it is a loss: credentials arrive as RPC parameters and scan
results go back the same way, so a guest has never had a reason to open the
database.

`TMPDIR` and `HOME` are **replaced**, not inherited. The values a host process
carries name directories outside every jail, so a guest reaching for a temp file
the ordinary way would fail on a permission error unrelated to anything it was
denied. `XDG_RUNTIME_DIR` is dropped for the same reason and has no per-guest
equivalent to point at.

### Why a descriptor per fetch rather than the cache root

A guest is long-lived — one process per plugin, serving every call for the life
of the daemon — and filesystem confinement is fixed at spawn. Granting the whole
cache would let one fetch read or overwrite every other fetch's scratch.

The host therefore opens exactly one work directory per `fetch_title`, sends it
over a Unix socket with `SCM_RIGHTS` on fd 3 (preserved through
`bookclerk-jail`), and the guest resolves `/proc/self/fd/N` (or `/dev/fd/N` on
macOS), where `N` is the received directory descriptor. The `cache_dir` string
on the wire remains for logging and for unconfined development; jailed guests
must use the descriptor.

A media job is confined far more tightly: one input file, one output directory,
per job, in a process that exits when the job does. See [media.md](media.md).

### Why a launcher and not self-confinement

`bookclerk-media-worker` confines itself, which is fine because that binary is
ours: only our own code could skip the call. A plugin guest inverts the
assumption — the guest binary *is* the untrusted part, so asking it to confine
itself asks the attacker to cooperate.

The host cannot apply the jail directly either. Both backends allocate, which is
unsafe after `fork` in a threaded process, and Landlock's `restrict_self` binds
the calling thread rather than the process, so a runtime's worker threads would
stay unconfined. `bookclerk-jail` is single-threaded and does nothing else, so it
has neither problem: it applies the policy, `exec`s the guest, and the guest
inherits restrictions it cannot refuse or inspect.

The policy crosses that boundary as JSON in `BOOKCLERK_JAIL_SPEC`, which the
launcher removes before the hand-off. A missing, malformed, or unsatisfiable spec
means **nothing runs** — the guest binary is never reached.

One consequence is worth stating plainly: the policy has to permit `execve`,
because that is how control is handed over. A guest can therefore run other
binaries inside its read allowlist, but it gains nothing by doing so. The
restrictions are inherited, irreversible, and `no_new_privs` is set, so setuid is
already neutral.

### Open descriptors, and why the launcher sweeps them

The allowlist is about paths, and an open descriptor is past the path check for
good. A guest reads one with no lookup at all, so a file the host still had open
across the spawn would be readable inside the jail whatever the policy said —
`master.key` included — and no grant could take it back.

So the launcher closes every descriptor above stdin, stdout and stderr before it
applies the policy, and refuses to hand over at all if it cannot enumerate them.
Nothing leaks today, because Rust opens files `O_CLOEXEC`; the point is that this
stops being a property of every library the host links, re-checked on every
dependency bump, and becomes a property of the jail.

### Declaring what a plugin needs

A manifest may ask for network reachability, and nothing else:

```toml
[sandbox]
network = "outbound"   # none | outbound | listen
```

| Value | Grants |
| --- | --- |
| `none` | no IP sockets at all |
| `outbound` (default) | outbound connections only — enough to call an API and download a file |
| `listen` | outbound plus a loopback callback listener on a kernel-assigned port |

`listen` exists for interactive OAuth: Audible's sign-in receives its
authorization code over loopback, so the Audible plugin declares it. On Linux the
grant is a bind on port 0 (the kernel picks); on macOS it is a bind and inbound
filtered to `localhost`. Either way a fixed, well-known port cannot be claimed.

Unrestricted network access is deliberately not expressible, and **the filesystem
allowlist cannot be widened from a manifest**. A manifest ships with the plugin
it describes, so anything it can ask for is something a hostile plugin can ask
for too.

### Isolation modes

```toml
[plugins]
isolation = "required"  # required | best-effort | off
# jail_bin = "/usr/local/bin/bookclerk-jail"
```

Environment overrides: `BOOKCLERK_PLUGIN_ISOLATION`, `BOOKCLERK_PLUGIN_JAIL`.

- **`required`** (default) — a plugin that cannot be jailed is not loaded. The
  error names the reason and the plugin is skipped; the rest of the host runs.
- **`best-effort`** — start the guest unconfined when the platform or the
  installation cannot support a jail, with a warning that says so per plugin.
- **`off`** — no jail. Development only.

The same three modes as `[media].isolation`, and the same reasoning: the tiers
differ in what they reach for, not in how a missing jail is handled. Confirm
what is in effect with `bookclerk config get plugins.isolation`.

| Platform | Backend | Filesystem | Syscalls | Network |
| --- | --- | --- | --- | --- |
| Linux | Landlock + seccomp-bpf | allowlist, ABI-probed | deny list | per-policy |
| macOS | Seatbelt (`sandbox_init`) | deny-default SBPL profile | — | per-policy |
| Windows | — | not yet | — | — |

**Windows has no plugin jail yet.** Confinement there is granted at
`CreateProcess` (AppContainer), which is not implemented, so `required` refuses
to load external plugins and says why at startup. First-party sources still work,
because default builds register them in-process. An operator who wants external
plugins on Windows today has to opt down explicitly:

```toml
[plugins]
isolation = "best-effort"  # guests run unconfined on Windows
```

In containers, Landlock needs a runtime that permits the `landlock_*` syscalls.
Docker's default seccomp profile has allowed them since 20.10.14; on an older
engine the syscalls are refused, `required` refuses to load plugins, and the
startup log names Landlock as the missing backend.

### Installing the launcher

The host looks for `bookclerk-jail` in this order:

1. `plugins.jail_bin` in `config.toml`
2. `BOOKCLERK_PLUGIN_JAIL`
3. `bookclerk-jail` beside the running executable

Each candidate is checked for existence, so a stale configured path fails loudly
instead of degrading to an unconfined guest. Build and ship it with the hosts:

```bash
cargo build --release -p bookclerk-cli -p bookclerkd \
  -p bookclerk-media-worker -p bookclerk-jail
```

The Docker images copy it into `/usr/local/bin` alongside `bookclerk` and
`bookclerkd`.

### Troubleshooting

**`refusing to run plugin <id> unconfined: bookclerk-jail not found in <dir>`** —
the launcher is not installed beside the host binary. Install it, point
`plugins.jail_bin` at it, or accept unconfined guests with
`isolation = "best-effort"`.

**`refusing to run plugin <id> unconfined: this host cannot confine a process`** —
no backend: Windows, a kernel without Landlock, or a container runtime that
blocks the Landlock syscalls. The message names the backend that came up short.

**`bookclerk-jail: these allowlist paths do not exist: …`** — a granted directory
vanished between planning and launch. Both backends reject a rule naming a
missing path, and the launcher refuses rather than silently narrowing the jail.

**A plugin fails on a path it used to read** — it is outside the four grants. A
guest that needs host data should be asking for it over RPC; the filesystem
allowlist is not negotiable from a manifest.

**Everything fails right after `handshake`** — check the guest's stderr, which is
inherited by the host and lands in the daemon log directly under the
`bookclerk-jail:` line reporting what was applied.

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
    data/                               # host-created: guest state, its HOME
    tmp/                                # host-created: guest scratch, its TMPDIR
```

`data/` and `tmp/` are created by the host at spawn, so a plugin archive should
not ship them; deleting one plugin's state means deleting those two directories.
They are keyed by plugin id under `$BOOKCLERK_FILES_DIR/plugins/<id>/` wherever
the binary itself was installed, which in the layout above is the same directory —
read-only to the guest apart from that writable pair.

Additional roots: `BOOKCLERK_PLUGIN_DIRS` (OS path list). A guest staged under one
of those still keeps its state under the files dir, so an upgrade that replaces
the staging tree does not take the plugin's state with it.

### `plugin.toml`

```toml
api_version = 1
id = "echo"
name = "Echo Integration"
kind = "integration"          # source | integration | output | database
command = "./bookclerk-plugin-echo-integration"
# args = ["--verbose"]

# Optional: what this plugin needs from its jail. Omitted means outbound-only.
[sandbox]
network = "none"              # none | outbound | listen

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

`command` may be absolute or relative to the manifest directory. An absolute
`command` is granted read access on its own, so a manifest may point at a binary
installed elsewhere.

`[sandbox]` is described under [the guest jail](#declaring-what-a-plugin-needs).
An unknown key or an unknown `network` value is a parse error rather than a
silent default — a typo in a security-relevant field must not read as "whatever
we would have picked".

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
| `event_poll` | Return `{ "users": [ ExternalUserDto, … ] }` — host polls after `start` and kicks off **core** workflows (e.g. claim tickets). The plugin stays oblivious to portal/tickets |

### Source capabilities

| Method | Notes |
| --- | --- |
| `login` | Password sources. Params: `plugin_data_dir`, marketplace/label/email/password. Result: `{ account, credentials? }` — host seals credentials (`provider = plugin id`) and upserts the account row |
| `login.start` / `login.complete` | OAuth sources (Audible). Start returns `{ session_id, url }`; complete returns `LoginResultDto` |
| `list_accounts` | Host-only (accounts table for this plugin id); plugin is not called |
| `scan` | Params: `plugin_data_dir`, filters, and host-injected `credentials` map (`account_id` → opaque JSON; **no** `library_db`). Result includes `books[]` DTOs; host upserts with `source` forced to plugin id |
| `fetch_title` | Host injects `credentials` from `encrypted_secrets`; plugin writes media under `cache_dir` and returns **`plain`** paths (DRM guests decrypt before return) |

Plugins must not open `library.db` or read `master.key`. Do not put Encrypted
content keys on the wire — decrypt in the guest when needed.

### Output plugins

`kind = "output"` guests implement [`StorageBackend`] over JSON-RPC. The host
never grants them the acquire cache or output library: `put_file` delivers the
local media file over the same side channel as source `fetch_title` directories
(fd 3 is the preserved `SCM_RIGHTS` socket; the open file descriptor arrives
over that socket immediately before the RPC). When no side channel is wired
(unconfined / best-effort), the host sends an absolute `local_path` in the RPC
params instead. S3 credentials and bucket config are injected on each RPC —
guests do not inherit `BOOKCLERK_AWS_*` or read `encrypted_secrets`.

| Method | Notes |
| --- | --- |
| `put` | Small objects (covers, sidecars): `{ key, data_base64, meta }` |
| `put_file` | Large audiobooks: host passes file fd, then RPC `{ key, meta }` |
| `get` / `exists` / `list` / `probe` / `copy` / `delete` / `touch_file` | Mirror in-process storage |

First-party S3 ships as `bookclerk-plugin-destination-s3`. When the guest is
discovered under `plugins/s3/` and `[output.s3].enabled = true`, the host
loads it at startup via [`load_external_destinations`] instead of the in-process
[`S3Backend`].

### Database plugins

`kind = "database"` guests implement the SeaORM proxy boundary over JSON-RPC.
The host opens the library through [`load_external_database`] /
[`open_library_store`]; SQLite receives `library.db` on fd 3 at `db.connect`.

| Method | Notes |
| --- | --- |
| `db.connect` | Open backend (SQLite: fd 3; D1/Postgres: injected credentials) |
| `db.ping` | Verify connectivity |
| `db.query` / `db.execute` | Forward SeaORM [`Statement`] payloads |

Built-in ids: `sqlite`, `d1`, `postgres` (match `[database].plugin`).

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

### First-party plugins (dual load via plugin host)

Audible, Libro.fm, Chirp, GraphicAudio, and Audiobookshelf ship as **external
plugins** under `crates/bookclerk-plugins/`. The host crate
`bookclerk-plugin` also registers the same adapters **in-process**
(`register_builtin_*` / `load_sources` / `load_integrations`) so `cargo run`
works without staging binaries. CLI/daemon call only those host helpers —
never store crates by name. Discovery skips an id that is already registered.

Guest binaries depend on **`bookclerk-plugin-sdk`** (+ their private store crate
for first-party). Third-party authors should depend on the SDK only — not
`bookclerk-plugin`, `bookclerk-library`, or `bookclerk-source`.

CI builds those plugin binaries and stages them with
`scripts/stage-first-party-plugins.sh` for integration tests (`BOOKCLERK_PLUGIN_ARTIFACTS`).
Artifacts are **not** published to crates.io / GitHub Releases yet.

Locally:

```bash
cargo build -p bookclerk-plugin-source-audible \
  -p bookclerk-plugin-source-libro \
  -p bookclerk-plugin-source-chirp \
  -p bookclerk-plugin-source-graphicaudio \
  -p bookclerk-plugin-integration-audiobookshelf \
  -p bookclerk-plugin-echo-integration
./scripts/stage-first-party-plugins.sh debug /tmp/bc-plugins
export BOOKCLERK_PLUGIN_DIRS=/tmp/bc-plugins
# or copy into $BOOKCLERK_FILES_DIR/plugins/
```

For **crates.io naming**, release-asset conventions, and install-without-Rust
(planned `bookclerk plugins search|install` + dashboard browser), see
[plugin-registry.md](plugin-registry.md).
