# Dynamic plugins

Bookclerk is built around pluggable **sources**, **destinations**, and
**integrations**. First-party **guest** implementations live under
`crates/bookclerk-plugins/{platform,optional}/` (one package per plugin). The
**host** runtime is
[`bookclerk-plugin-host`](../crates/bookclerk-plugin-host) — discovery, spawn,
jail, consent, and Workers RPC — distinct from the runtime install tree
`$BOOKCLERK_FILES_DIR/plugins/`.

The product ABI is **Workers RPC** at `api_version = 1` (no `protocol` key; no
JSON-RPC 2.0 as the product ABI). Decision record:
[`docs/adr/plugin-workers-rpc-workerd.md`](adr/plugin-workers-rpc-workerd.md).
Authoritative schema:
[`crates/bookclerk-plugin-abi/schema/abi.json`](../crates/bookclerk-plugin-abi/schema/abi.json).

Authors implement the branded guest base **`BookclerkPlugin`** via a minimal
language SDK. Each SDK covers **both** `native` and `workerd`, plus in-process
author tools (`check` / `fmt` / `sync-embed` / `package` / `smoke`) that are
**self-contained** for that language (vendoring the `BookclerkPlugin` embed
and, for Python Workers, the required compatibility flags). Workerd `smoke`
downloads the pinned Cloudflare `workerd` binary and runs handshake/health
without a built Rust `bookclerk-workerd` launcher — see
[packaging.md](packaging.md#plugin-author-tools-check--fmt--sync-embed--package--smoke).

On native guests, `BookclerkPluginGuest.serve` is the stdin/stdout Workers RPC
runner — authors still subclass (or implement) `BookclerkPlugin`.

| Language | Package | Notes |
| --- | --- | --- |
| TypeScript / Node | [`@bookclerk/plugin-sdk`](../packages/plugin-sdk/) | `/workerd` + `/native` both export `BookclerkPlugin`; native adds `BookclerkPluginGuest` |
| Python | [`bookclerk-plugin-sdk`](../packages/plugin-sdk-python/) | `BookclerkPlugin` + `BookclerkPluginGuest`; workerd `BookclerkPlugin` |
| Rust | [`bookclerk-plugin-sdk`](../crates/bookclerk-plugin-sdk/) | `BookclerkPlugin` trait + `BookclerkPluginGuest`; workerd embed + `wasmBookclerkPlugin` |

Runtimes in `plugin.toml`:

| `runtime` | How it runs |
| --- | --- |
| `native` | OS binary speaking the same Workers RPC ABI (spawned through `bookclerk-jail`) |
| `workerd` | Author `modules/` loaded by first-party [`bookclerk-workerd`](../crates/bookclerk-workerd/) — **one jail + one isolate per plugin** (local embed, not Cloudflare cloud execution) |

For the product overview see the [documentation index](README.md). Built-in
storefronts: [sources.md](sources.md). Audiobookshelf / Connect:
[integrations.md](integrations.md). Publishing / crates.io taxonomy and
standalone author repos: [plugin-registry.md](plugin-registry.md).

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
| **In-process fallback** | When a platform guest is missing or fails to start, hosts fall back to logic in `bookclerk-library` / `bookclerk-storage` |
| **`bundled-plugins`** | Optional host feature linking storefronts in-process (dev only; omit for release packaging) |
| **`BookclerkPlugin`** | Branded guest contract (TS/Python class / Rust trait) on both native and workerd; app code never depends on bare platform entrypoints |
| **`BookclerkPluginGuest`** | Native-only stdio Workers RPC runner that dispatches to a `BookclerkPlugin` (sibling of low-level `PluginGuest`) |

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
cargo test-staged                       # handshake smoke
```

Add `--release` to any alias for release builds. Override staging dir with
`BOOKCLERK_PLUGIN_ARTIFACTS`. Forward host args after `--` (e.g. `cargo dev -- --help`).

Optional in-process iteration (no staging): build hosts with
`--features bundled-plugins` on `bookclerk-cli` / `bookclerkd`.

Reference Echo examples (distinct plugin ids):

Plugin **ids are globally unique across kinds** (source / integration / output /
database). The grammar is strict and non-lossy: lowercase `[a-z0-9_]{2,32}` with
no leading/trailing `_` and no `__` (same rule as the crates.io `{id}` segment).
Invalid characters are rejected at manifest load / install — never rewritten —
so values like `a/b` and `a_b` cannot collide after sanitization.

| Path | Runtime |
| --- | --- |
| [`examples/plugins-echo-native-rust`](../examples/plugins-echo-native-rust/) | native Rust |
| [`examples/plugins-echo-native-node`](../examples/plugins-echo-native-node/) | native Node SEA |
| [`examples/plugins-echo-native-python`](../examples/plugins-echo-native-python/) | native Python |
| [`examples/plugins-echo-workerd-ts`](../examples/plugins-echo-workerd-ts/) | workerd TypeScript |
| [`examples/plugins-echo-workerd-python`](../examples/plugins-echo-workerd-python/) | workerd Python Workers |
| [`examples/plugins-echo-workerd-rust`](../examples/plugins-echo-workerd-rust/) | workerd Rust/Wasm |
| [`examples/plugins-echo-workerd-fetch`](../examples/plugins-echo-workerd-fetch/) | workerd outbound `*.example.com` + logo |

```bash
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-ts && npm ci && npm run typecheck
```

## Why subprocesses?

Content sources and integrations need their own async runtimes, HTTPS clients,
and OAuth flows. Loading foreign `cdylib`s into the host process is fragile
across Rust/Tokio versions. A child process (native binary or
`bookclerk-workerd` under jail) gives crash isolation, independent releases, and
a stable ABI. Workerd is an **authoring/runtime** choice for portable JS/TS
modules — the jail remains the security boundary (see the ADR).

## Trust model (external plugins are untrusted)

External plugins run as a **separate OS process**, confined by the host to the
directories it hands them ([the guest jail](#the-guest-jail)). The ABI boundary
is narrow on top of that:

| Host guarantees | Detail |
| --- | --- |
| No library DB path (sources / integrations / outputs) | `library.db` is never passed on the wire to those kinds — and not reachable if it were. **Database** guests receive the SQLite file only via the fd-3 side channel (or `sqlite_path` when unconfined) at `dbConnect` |
| No files-dir root | Plugins get `plugin_data_dir` (`…/plugins/<id>/data`) and a per-fetch work directory (descriptor) — not `master.key` or the download cache root |
| Env scrub | Child spawn uses `env_clear` + a small allowlist (`PATH`, locale, …). `BOOKCLERK_*`, `AWS_*`, tokens, and DB URLs are not inherited; `HOME` and `TMPDIR` are replaced with the guest's own directories |
| Host-mediated secrets | `login` returns `{ account, credentials }`; host seals into `encrypted_secrets` with `provider = plugin id`. `scan` and `fetchTitle` receive those blobs from the host |
| Host-mediated library writes | `scan` returns book DTOs; host upserts with `source` forced to the plugin id. Account listing for a plugin id is answered from the host accounts table |
| Scoped identity | Plugin cannot claim another storefront’s `source` / `provider` |
| Network consent | Operator runs `bookclerk plugins approve` before enable; the **same covering grant is required again at every external spawn** and at privileged delivery (`config` / `secrets` / `work_fs` / `oauth`). **Workerd** guests enforce `capabilities.network.domains` as the isolate hostname allowlist (**initial host only**, IDNA-normalized; redirect hops stay free by design). **Native** guests with `mode = "outbound"` get coarse jail internet — **no hostname filter**; `domains` must not be declared |

First-party guests ship under `crates/bookclerk-plugins/` with the guest SDK
contract. Host binaries (`bookclerk`, `bookclerkd`) depend on
**`bookclerk-plugin-host`** only — not on individual store crates. Optional
`bundled-plugins` features on the hosts call `register_builtin_*` to link
first-party libraries in-process for faster Rust iteration; release builds omit
that feature and load staged guests from `plugins/` instead. Discovered
external copies of the same id are skipped when an in-process adapter is already
registered. After registration, hosts talk **only**
through `ContentSource` /
`Integration` (login, scan, fetch, import, revoke, inspect, plus catalog
`searchCatalog` / `catalogDetail` / `expandCandidates` / `purchaseHint` /
`listDeals` for Discover). Sources always return `PlainFetch` (`SourceFetch` is
an alias) —
DRM (Adrm/CENC) is decrypted inside the Audible plugin before the host sees
media. Guest `fetchTitle` carries optional `pdfUrl`; catalog methods are on the
Workers RPC wire for external guests.

Enabling a third-party plugin still means running that guest as the Bookclerk
user, inside the jail below — review and **approve** capabilities before
enabling.

## Shipping without a store

Both hosts carry one optional feature per in-process plugin (`bundled-plugins`
enables the full set). **Default builds link no storefront** (external guests
only):

```bash
# Default: external guests only (release packaging).
cargo build --release -p bookclerk-cli -p bookclerkd

# Everything except Audible (still in-process, opt-in).
cargo build -p bookclerk-cli -p bookclerkd --features bundled-plugins \
  --no-default-features \
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
`scripts/check-store-free-hosts.sh` asserts it in CI: default hosts must link
no plugin package and reach no cipher crate. Opt-in `--features bundled-plugins`
must still link Audible for in-process dev. A shared crate that grew an `aes`
dependency fails the default-host check.

Users of a store-free build can still add any storefront back as an external
guest, since discovery is independent of these features. That is a deployment
choice made by whoever installs the plugin, not by whoever shipped the binary.

## The guest jail

Every external guest is started by **`bookclerk-jail`**, a small launcher that
applies a confinement policy to itself and then `exec`s the plugin process
(native binary **or** `bookclerk-workerd`). What it grants is decided entirely
by the host. Workerd is not a substitute for the jail — one jail + one isolate
per plugin.

A guest gets four paths and nothing else:

| Path | Access | Also known to the guest as |
| --- | --- | --- |
| its install directory | read-only | `cwd` |
| `…/plugins/<id>/data` | read/write | `HOME`, and `plugin_data_dir` on the wire |
| `…/plugins/<id>/tmp` | read/write | `TMPDIR` / `TEMP` / `TMP` |
| one fetch work directory at a time | write (via descriptor) | passed on fd 3 immediately before each `fetchTitle` |

Plus the system read paths every process needs to start (the loader, shared
libraries, the CA bundle — including `/var/lib/ca-certificates` on
openSUSE/SLE where `/etc/ssl` only symlinks there — resolver config) and a
writable `/dev/null`.

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

The host therefore opens exactly one work directory per `fetchTitle`, sends it
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

A manifest declares network and host bindings under **`[capabilities]`** — not
a free-form filesystem widen.

**Workerd** (hostname-filtered outbound):

```toml
runtime = "workerd"
# …

[capabilities.network]
mode = "outbound"
domains = ["api.example.com", "www.example.com"]   # required; isolate allowlist
```

**Native** (coarse jail outbound — **no domain list**):

```toml
runtime = "native"
command = "./my-plugin"

[capabilities.network]
mode = "outbound"   # deny | outbound — do NOT set `domains`
```

| Network `mode` | Native | Workerd |
| --- | --- | --- |
| `deny` | no IP sockets (`NetPolicy::Deny`, including OAuth listen) | OS jail stays `OutboundListen` for the RPC bridge; isolate `globalOutbound` → blocked (grant is isolate-enforced; see the ADR) |
| `outbound` | coarse jail internet (`NetPolicy::Outbound`, or `OutboundListen` with oauth) — **not** hostname-filtered | isolate egress allowlist; **`domains` required** |

`capabilities.network.domains` is **workerd-only**. Declaring `domains` on a
native plugin is a validate/schema error. Do **not** invent domains on native
plugins for Settings favicons — use optional top-level `logo` instead. Native
storefronts (Audible, Libro, Chirp, GraphicAudio), destinations (S3), and
databases (Postgres, D1 HTTP) that need the network use `mode = "outbound"` and
dial with normal OS sockets under the jail. There is no host HTTP/TCP domain
proxy for native guests: AppContainer
blocks loopback `HTTP_PROXY`, and an HTTP-only IPC mediator cannot carry
Postgres TCP, the AWS SDK, or libraries that embed their own HTTP client.

When you need enforceable hostname allowlists, ship a **workerd** plugin. The
operator still **approves** native `outbound` (with an explicit warning that the
guest can reach the open internet).

Workerd egress matching (shared `EgressPolicy` + `bridge/egress.js`):

- **Initial host only.** The first request URL's host must be on
  `capabilities.network.domains` (with `*.` prefix wildcards). Matching uses
  **IDNA ToASCII** on both the request host and allowlist patterns; percent-encoded
  hosts and failed IDNA are **rejected** (fail closed). Unicode and Punycode forms
  of the same name match after normalization.
- **Redirect hops stay free (intentional).** After an allowed initial host,
  `Location` redirects are followed up to `maxRedirects` **without** re-checking
  the domain allowlist. Do not treat hop hosts as consented domains — operators
  approve the initial allowlist only. Cross-origin redirects drop `Authorization`
  (Fetch CORS non-wildcard request-header) plus `Cookie` / `Cookie2` /
  `Proxy-Authorization` as defense in depth. Method/body follow Fetch
  HTTP-redirect fetch: 301/302 convert **POST→GET** only; 303 converts
  non-GET/HEAD→GET; 307/308 preserve method/body. `AbortSignal` and other
  RequestInit metadata survive hops.
- **Python + outbound.** Workerd Python guests also require the Pyodide/CDN hosts
  (`cdn.jsdelivr.net`, `pypi.org`, `files.pythonhosted.org`) in the consent grant;
  materialize uses the same set and does not silently widen beyond it.
- **`[workerd].limits`.** Optional `cpu_ms` / `subrequests` are **clamped by the
  host** (defaults `30000` / `50` when unset or `0`; hard caps `120000` /
  `1000`). Local workerd **cannot** Cap'n Proto-emit Cloudflare-style
  `cpuMs` / `subRequests` — Bookclerk injects the clamped `subrequests` budget
  into `EGRESS_POLICY`. The egress bridge enforces it **per egress invocation**
  (one plugin `fetch()` plus that call's redirect hops → **429** when
  exceeded), matching Cloudflare's *per-invocation* subrequest budgeting rather
  than an isolate-lifetime / module-scope counter. Bookclerk plugins are
  long-lived across many host RPCs; aggregating subrequests across multiple
  plugin `fetch()` calls inside one RPC would need a CF-comparable invocation
  unit and is deferred. `cpu_ms` is clamped and logged at isolate start;
  OS-jail CPU enforcement is a follow-up
  (see [#143](https://github.com/fritz-fritz/bookclerk/issues/143)).

`bindings.oauth = true` (with outbound network) is how storefronts declare an
OAuth-style callback need. The **host** owns the browser-facing TCP listener and
forwards connections over IPC (see [Interactive listeners](#interactive-listeners-oauth-and-similar));
native guests with oauth also receive jail listen rights for that tunnel.

Unrestricted network access is deliberately not expressible, and **the filesystem
allowlist cannot be widened from a manifest**. A manifest ships with the plugin
it describes, so anything it can ask for is something a hostile plugin can ask
for too — consent and review are the operator controls.

Optional `[capabilities.methods]` lists RPC method names for discovery/consent
UI (e.g. `handshake`, `health`, `onEvent`, `cli`).

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

| Platform | Backend | Filesystem | Syscalls | Network | Memory / CPU / PIDs |
| --- | --- | --- | --- | --- | --- |
| Linux | Landlock + seccomp-bpf | allowlist, ABI-probed | deny list | per-policy | cgroup v2 best-effort (`memory.max` / `cpu.max` / `pids.max`) when Spec sets limits |
| macOS | Seatbelt (`sandbox_init`) | deny-default SBPL profile | — | per-policy | unsupported (FS/net only; no fake enforcement) |
| Windows | AppContainer | spawn-time ACL allowlist | — | capability SIDs | Job Object (Spec fields override label heuristics) |

### Windows confinement

Windows cannot confine a process after it has started. `bookclerk-jail` therefore
`CreateProcess`es the guest into an AppContainer (via
`bookclerk_sandbox::spawn::run_appcontainer`), ACLs the policy's **explicit**
read/write paths for a **per-launch** Package SID, maps `NetPolicy` to
capability names (`internetClient`, `privateNetworkClientServer`, …), places the
guest in a kill-on-close Job Object, and proxies stdio until the guest exits.

#### Job Object launch ordering

Bookclerk owns CreateProcess (not rappct’s assign-after-run path). When Job
membership is required, the launcher prefers `PROC_THREAD_ATTRIBUTE_JOB_LIST`
so assignment happens before any guest instruction runs; otherwise it uses
`CREATE_SUSPENDED` → configure Job (`KILL_ON_JOB_CLOSE` + resource limits) →
`AssignProcessToJobObject` → `ResumeThread`. Any failure after CreateProcess
terminates the child and closes process, thread, pipe, and Job handles.
Descendants cannot outlive jail/profile/ACL cleanup.

#### Profile paths

`GetAppContainerFolderPath` is authoritative when under Known Folder
LocalAppData `\Packages\`. Docs describe `Packages\<moniker>\AC`; Windows CI
measures `Packages\<package-SID>` (with `\AC` used when that child exists).
Bookclerk fails closed if the API fails or returns a path outside Packages — it
does **not** synthesize a Packages path. Child cwd / `LOCALAPPDATA` use that
folder; `TEMP`/`TMP` use `<folder>\Temp`.

#### ACL mutations

Bookclerk **never mutates DACLs under protected OS-managed roots** (Windows /
System32 / WinSxS / Program Files / ProgramData\Microsoft, resolved via
`GetWindowsDirectoryW` / `GetSystemDirectoryW` / Known Folders). Ambient OS
runtime access (loading system DLLs / `cmd.exe`) comes from existing OS ACLs
such as ALL APPLICATION PACKAGES. Explicit policy paths are temporarily ACLed
for the Package SID. Cross-process DACL read/modify/write is serialized with the
named mutex `Local\bookclerk-dacl-tx` (plus an in-process lock). Revoking an ACE
does **not** invalidate handles the guest already opened.

Plugin hosts create the AppContainer profile up front, put
`windows_profile_name` on the jail `Spec`, and delete the profile when the
plugin client drops. Media jobs leave that field unset so the jail creates a
unique profile per job.

Per-fetch / upload / sqlite paths are not in the spawn allowlist. On Unix the
host passes an open descriptor over `SCM_RIGHTS`; on Windows it temporarily ACLs
the path for the Package SID, puts the path on the RPC wire, and revokes
the ACE when the RPC returns.

#### Interactive listeners (OAuth and similar)

`capabilities.bindings.oauth = true` declares that the storefront needs an
OAuth-style callback. The **host** owns the browser-facing TCP listener and
forwards each connection over a duplex IPC tunnel (Unix socket under the plugin
scratch dir, or a Windows named pipe) into the guest, which still runs its HTTP
stack (Audible LoginServer). `loginStart` carries `callback_ipc` +
`callback_public_base`; the guest must not bind TCP when those are set.

On Windows the named pipe is created with a Package-SID DACL (plus SYSTEM /
Administrators / Creator Owner) and a Low mandatory integrity label so the
AppContainer guest can open it; remote clients are rejected. Unix sockets stay
mode `0600` under the plugin scratch dir.

This is required on Windows AppContainer (host↔guest loopback is blocked even
with Full caps / CheckNetIsolation) and is used on all OSes for a uniform
contract. Do not enable CheckNetIsolation. `--external` paste flows remain
available as a fallback.

#### Availability

Plugin Jobs get conservative memory / active-process (and optional CPU) limits;
media workers get higher defaults. When a jail `Spec` carries
`memory_bytes` / `active_processes` / `cpu_rate_percent` (percent of **one
logical CPU**; values above 100 request multi-core bandwidth up to
`logical_cpus × 100`), those values override the label heuristics on Windows
(Job `CpuRate` is scaled by core count so the meaning matches Linux cgroup
`cpu.max`; Job memory is **job-wide** commit charge, matching Linux
`memory.max`). Operator grants expose an **extra** process/thread budget
(`extraProcesses`, default **2**) above launcher overhead (native **1**,
workerd **2**); Spec `active_processes` = overhead + extra (capped at 64).
Workerd consent does not edit process budget (host-managed headroom). Workerd
guests use isolate `cpu_ms` for the script budget; their jail CPU rate comes
from the host default / `[plugins.jail]` per-jail ceiling (default **80**)
rather than a per-plugin `cpuRatePercent`. On Linux the Spec fields are applied
best-effort via a dedicated cgroup v2 child (never written onto a shared parent
slice); on macOS Seatbelt they are ignored (documented as unsupported — FS/net
only). `[plugins.jail].cpu_rate_percent` is a **per-jail ceiling** only (not a
cumulative reservation; default 80). Quotas cap how fast a guest may burn CPU;
if many plugins’ ceilings sum above host capacity, the OS scheduler shares
cycles among runnable guests. Each plugin's `data/` and `tmp/` directories are
capped at **512 MiB each**: the host measures them at jail plan (spawn/reload)
and again before write-heavy RPC side-passes (fetch directory, upload file,
database file grants). Over budget refuses the operation, kills the guest, and
quarantines the client until restart. RPC timeouts and framing violations
likewise kill and quarantine. Stdin proxying does not block jail exit after the
guest terminates.

#### Trust vs sandbox

Plugins are untrusted relative to Bookclerk’s master key, database, unrelated
files, and other plugins. A source plugin is necessarily trusted with credentials
and content deliberately sent to it; a network-capable plugin can exfiltrate
anything it was given. The sandbox reduces blast radius; it does not authenticate
publisher intent. SHA-256 digests verify artifact integrity, not publisher
identity — unsigned/unverified installs require explicit approval.

`allow_exec` is not separately enforceable at CreateProcess on Windows; path
ACLs and low integrity remain the boundary. Descendants inherit the AppContainer
token and Job Object membership. Regular AppContainer (not LPAC) is the default
spawn path; LPAC ambient restrictions are not currently applied.

Self-confinement (`Policy::confine_current_process`) remains unsupported on
Windows — media workers use the same spawn-side AppContainer path through
`bookclerk-jail`.

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
instead of degrading to an unconfined guest. Build and ship it with the hosts
(and `bookclerk-workerd` when shipping workerd guests):

```bash
cargo build --release -p bookclerk-cli -p bookclerkd \
  -p bookclerk-media-worker -p bookclerk-jail -p bookclerk-workerd
```

The Docker images copy helpers into `/usr/local/bin` alongside `bookclerk` and
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

**Enable / spawn refused: run `bookclerk plugins approve` first** — consent grant is
missing or the plugin widened `capabilities` since the last grant. Re-approve,
then `bookclerk plugins enable <id>` (spawn also fails closed until the grant
covers the current manifest).

## Two files, two jobs

| File | Role |
| --- | --- |
| `plugin.toml` (next to the binary or `modules/`) | **Install / discovery** — id, kind, runtime, command or `[workerd]`, capabilities |
| `config.toml` (`[sources.<id>]` / `[integrations.<id>]`) | **User settings** — `enabled`, opaque knobs |

The plugin (or its installer) drops a directory under a search root. Bookclerk
scans for `plugin.toml`, spawns the native `command` or `bookclerk-workerd`, and
passes the matching main-config table on `handshake`. Users never put `command`
in `config.toml`.

## Layout

Native:

```text
$BOOKCLERK_FILES_DIR/plugins/
  echo/
    plugin.toml
    bookclerk-plugin-echo-native-rust   # executable
    data/                               # host-created: guest state, its HOME
    tmp/                                # host-created: guest scratch, its TMPDIR
```

Workerd (script archive — no per-OS binary required):

```text
$BOOKCLERK_FILES_DIR/plugins/
  echo/
    plugin.toml
    modules/
      index.js
    data/
    tmp/
```

`data/` and `tmp/` are created by the host at spawn, so a plugin archive should
not ship them; deleting one plugin's state means deleting those two directories.
They are keyed by plugin id under `$BOOKCLERK_FILES_DIR/plugins/<id>/` wherever
the guest itself was installed — read-only to the guest apart from that writable
pair.

Additional roots: `BOOKCLERK_PLUGIN_DIRS` (OS path list). A guest staged under one
of those still keeps its state under the files dir, so an upgrade that replaces
the staging tree does not take the plugin's state with it.

### `plugin.toml`

Native Echo:

```toml
api_version = 1
id = "echo"
name = "Echo Integration"
kind = "integration"          # source | integration | output | database
version = "0.1.0"
# logo = "https://example.com/icon.png"   # or "assets/logo.png" (host-served)
runtime = "native"
command = "./bookclerk-plugin-echo-native-rust"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true

[capabilities.methods]
list = ["handshake", "health", "diagnose", "onEvent", "cli"]

# Optional: CLI help without spawning (handshake / cliDescribe win at invoke)
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

Workerd Echo (TypeScript / Python / Rust-Wasm under
[`examples/plugins-echo-workerd-*`](../examples/); TOML shape matches TS):

```toml
api_version = 1
id = "echo"
name = "Echo Integration"
kind = "integration"
version = "1.0.0"
runtime = "workerd"

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

[capabilities.bindings]
config = true
plugin_kv = true
```

There is **no** `protocol` key. `api_version` must be `1`. Optional top-level
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

This is separate from handshake **`BrandDto.icon_url`**: that field is the live
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
`[workerd]` + `modules/` into a real isolate. If `compatibility_date` is newer
than the Bookclerk pin, the host **warns** and still loads. The pin itself is
bumped on a **7-day publish cooldown** by CI — see
[packaging.md](packaging.md#cloudflare-workerd-pin).

Unknown capability keys are parse errors rather than silent defaults — a typo in
a security-relevant field must not read as "whatever we would have picked".

Two plugins that claim the same `id` for the same `kind` are a **hard startup
error** (CLI/daemon exit). The same id across different kinds (e.g. a source and
an integration both named `echo`) is allowed. An external id that collides with
a first-party plugin of the same kind is also rejected.

## Consent before enable (and every spawn)

Third-party (and newly installed) plugins require an explicit permission grant
before **enable** and again before **every external spawn**. Privileged delivery
also checks individual bindings: handshake `config`, host-injected secrets,
`work_fs` side-channel / ACL passes, and OAuth callback proxy setup.

**Operator Settings** shows a branded consent dialog when enabling a plugin that
is not yet covered. The dialog starts from the manifest baseline and lets the
operator **widen or narrow** domains/bindings/flags, network mode, workerd
`cpuMs`/`subrequests`, and shared `diskMib` (host-capped). Daemon APIs:

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/plugins/{id}/consent` | Request surface, `covered`, optional `brand` / `limits` |
| `POST` | `/api/plugins/{id}/consent` | `{ "approve": true, "grant"?: { … } }` — omit `grant` for full request |

CLI remains available (always approves the full request):

```bash
bookclerk plugins approve echo          # interactive summary of network/bindings
bookclerk plugins approve echo --yes    # non-interactive
bookclerk plugins enable echo
```

Platform guests (`sqlite`, `local`) skip the consent UX and are enabled by
default; the host auto-persists a covering grant on first spawn when their
manifest stays within the installer envelope (deny network, `config` /
`work_fs` only).

Grants are persisted under `$BOOKCLERK_FILES_DIR/plugin-grants.json`. The
manifest consent request is a **baseline**, not a hard ceiling: operators may
**widen or narrow** domains, bindings, flags, network mode, workerd budgets, and
per-plugin disk / jail memory / CPU rate / extra process budget (`diskMib`,
`memoryMib`, `cpuRatePercent` and `extraProcesses` for **native** only). Host
hard caps still apply (`WorkerdLimits` maxes, disk/memory max 4096 MiB, CPU rate
up to `logical_cpus × 100` one-core percent, extra processes 62 / absolute PIDs
64, known bindings). Workerd plugins use isolate `cpuMs` instead of per-plugin
`cpuRatePercent`; jail CPU for workerd follows the host default /
`[plugins.jail]` per-jail ceiling (default 80). Process headroom for workerd is
host-managed (overhead 2 + default extra 2). Bookclerk does **not** guarantee
plugin behaviour if overrides remove capabilities the guest needs. Domain
allowlists are enforced for **workerd** guests (via `BOOKCLERK_WORKERD_GRANT_*`
→ `EGRESS_POLICY`); **native** guests get OS-jail allow-or-deny for network (no
hostname filter). Jail Spec memory/CPU/PID ceilings and disk budgets apply to
**both** runtimes. Redirect following does **not** expand the consented domain
list (hops stay free by design; only the initial host is allowlisted).

Approving a **native** plugin with `mode = "outbound"` shows an explicit warning
that networking is **not** hostname-filtered.

Global confinement knobs (Settings → Confinement, or `config.toml`):

| Key | Purpose |
| --- | --- |
| `plugins.isolation` | `required` / `best-effort` / `off` |
| `media.isolation` | same tiers for media-worker |
| `plugins.jail.memory_mib` | optional Spec memory ceiling |
| `plugins.jail.cpu_rate_percent` | per-jail CPU ceiling as integer percent of one core (default **80**; 100 = 1.00 core; UI edits cores to two decimals; max = cores×100; OS shares if oversubscribed) |
| `plugins.jail.extra_processes` | ceiling on extra processes/threads beyond launcher overhead (default **2**; Spec `active_processes` = overhead + extra) |

Guest filesystem access remains install read-only plus host-managed
`plugins/<id>/data` and `plugins/<id>/tmp` — not a free-form widen.

**Deferred (discovery/install):** a content hash bound to the grant so a
different binary under the same id cannot keep an old grant forever. End goal
once install/upgrade exists: on upgrade, **refresh the registered hash without
re-prompting** when capabilities did not widen; re-prompt only when the
capability scope expands (`grant_covers` already encodes that rule). Do not
expect spawn-time hash checks in this release.

## Enabling and settings in `config.toml`

Plugin `id` must match a config table. **External integrations default to
disabled**; sources follow the usual `[sources.<id>]` rules (missing → enabled).
`plugins enable` still refuses until a grant exists.

```toml
[integrations.echo]
enabled = true
# greeting = "hi"   # opaque knobs → handshake config

[sources.my_store]
enabled = true
# … opaque knobs …
```

## ABI (api_version = 1, Workers RPC)

Host ↔ plugin: Workers RPC method calls with structured (camelCase) payloads.
The schema in
[`crates/bookclerk-plugin-abi/schema/abi.json`](../crates/bookclerk-plugin-abi/schema/abi.json)
is authoritative; Rust (`bookclerk-plugin-abi` / `bookclerk-plugin-sdk`) and
TypeScript (`@bookclerk/plugin-sdk`) are generated projections of that contract.
Method names on the wire are **camelCase** (`onEvent`, `cliInvoke`, `fetchTitle`,
…). Guests outside the host’s supported API version fail handshake cleanly.

Native guests implement `BookclerkPlugin` and speak the ABI over stdio via
`BookclerkPluginGuest::serve` / `BookclerkPluginGuest.serve`. Workerd guests
expose the same methods on a `BookclerkPlugin` entrypoint loaded by
`bookclerk-workerd` (no separate guest runner — the isolate hosts the class).

### Reverse channel (`HOST.notify`)

Workerd guests may call `env.HOST.notify(event)` with a `PluginToHost`-style
payload. `bookclerk-workerd` wires the isolate `HOST` binding to a loopback
HTTP callback: events are POSTed to the launcher, buffered in memory for the
session, and logged (event `type` + size only — not the full JSON body).

Bridge loopback RPC (`/rpc`, `/health`) and the notify reverse channel share a
**per-isolate bearer token** (`BRIDGE_TOKEN` Cap’n Proto binding on both the
bridge and host workers). The launcher generates the token, injects it into the
workerd config, and sends `Authorization: Bearer …` on every bridge request;
`host_stub.js` does the same for notify. Requests without a matching bearer are
rejected (`401`). Notify parsing also requires a valid `Content-Length` (hard
max 64 KiB), limits concurrent accepts, and caps the in-memory event buffer
(drop-oldest when full).

Native stdio guests already have a reverse path on the RPC framing; this workerd
path is the minimal equivalent until the host fans events into
integrations/jobs.

### Common

| Method | Purpose |
| --- | --- |
| `handshake` | Negotiate version, id, kind, capabilities, brand |
| `shutdown` | Graceful teardown |
| `health` | Connectivity / config check |
| `diagnose` | Human-readable CLI probe lines |
| `cliDescribe` | Declared CLI command schema (`CliSchema`) |
| `cliInvoke` | Run a declared command (`CliInvokeParams` → `CliInvokeResult`) |

Handshake params include `{ "apiVersion": 1, "config": {…} }` — the plugin’s
`[sources.<id>]` / `[integrations.<id>]` table from **main** `config.toml` as JSON
(empty object if the table is missing).

Optional handshake field `cli` may embed the same schema as `cliDescribe`. Prefer
advertising capability `cli` and implementing both methods. You may also mirror
the schema in `plugin.toml` under `[cli]` so `bookclerk plugins <id> --help`
works without spawning the plugin; at invoke time handshake / `cliDescribe`
remain authoritative.

### Plugin CLI

Host commands that apply to every plugin stay on Bookclerk verbs
(`plugins list|info|diagnose|approve|enable|disable`, `integrations …`,
`auth …`). Plugin-specific commands are declared and invoked as:

```bash
bookclerk plugins <plugin-id> <command> [args…]
```

Example schema (JSON / handshake `cli` / `cliDescribe`):

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

`cliInvoke` params: `{ "command": "ping", "args": { "message": "hi" } }`.
Result: `{ "exitCode": 0, "stdout": "…", "stderr": "…", "json": … }`.

### Integration capabilities

Advertise in `handshake.capabilities`: `start`, `onEvent`, `health`,
`diagnose`, `scanLibrary`, `syncListening`, `authenticateUser`, `cli`.

| Method | Notes |
| --- | --- |
| `start` | Background watchers |
| `onEvent` | `{ "type": "book_acquired"\|"external_user_observed", "payload": … }` |
| `scanLibrary` | `{ "force": bool }` |
| `syncListening` | Return listening progress snapshots; host upserts tagged with plugin id |
| `authenticateUser` | `{ "username", "password" }` → external user |
| `pollEvents` | Return observed external users — host polls after `start` and kicks off **core** workflows (e.g. claim tickets). The plugin stays oblivious to portal/tickets |

### Source capabilities

| Method | Notes |
| --- | --- |
| `login` | Password sources. Params include `pluginDataDir`, marketplace/label/email/password. Result: `{ account, credentials? }` — host seals credentials (`provider = plugin id`) and upserts the account row |
| `loginStart` / `loginComplete` | OAuth sources (Audible). Start returns `{ sessionId, url }`; complete returns login result |
| `scan` | Params include `pluginDataDir`, filters, and host-injected `credentials` map (`accountId` → opaque JSON; **no** library DB path). Result includes `books[]` DTOs; host upserts with `source` forced to plugin id |
| `fetchTitle` | Host injects `credentials` from `encrypted_secrets`; plugin writes media under the work directory and returns **plain** paths (DRM guests decrypt before return) |

Plugins must not open `library.db` or read `master.key`. Do not put Encrypted
content keys on the wire — decrypt in the guest when needed.

### Output plugins

`kind = "output"` guests implement storage over Workers RPC. The host
never grants them the acquire cache or output library: `putFile` delivers the
local media file over the same side channel as source `fetchTitle` directories
(fd 3 is the preserved `SCM_RIGHTS` socket; the open file descriptor arrives
over that socket immediately before the RPC). When no side channel is wired
(unconfined / best-effort), the host sends an absolute local path in the RPC
params instead. S3 credentials and bucket config are injected on each RPC —
guests do not inherit `BOOKCLERK_AWS_*` or read `encrypted_secrets`.

| Method | Notes |
| --- | --- |
| `put` | Small objects (covers, sidecars): key + data + meta |
| `putFile` | Large audiobooks: host passes file fd, then RPC key + meta |
| `get` / `exists` / `list` / `probe` / `copy` / `delete` / `touchFile` | Mirror in-process storage |

First-party S3 ships as `bookclerk-plugin-destination-s3`. When the guest is
discovered under `plugins/s3/` and `[output.s3].enabled = true`, the host
loads it at startup via external destination loading instead of the in-process
S3 backend.

### Database plugins

`kind = "database"` guests implement the SeaORM proxy boundary over Workers RPC.
Engine connect/migrate/proxy code lives in the guest
(`bookclerk-plugin-database-sqlite` (and optional d1/postgres guests) modules); the host does not link SQL engines.
The host opens the library through the external database loader (guest required —
no in-process fallback). SQLite receives `library.db` on fd 3 at `dbConnect`.

| Method | Notes |
| --- | --- |
| `dbConnect` | Open backend via tagged connect params (`backend`: `sqlite` / `d1` / `postgres`); returns dialect (SQLite: fd 3; D1/Postgres: host-injected credentials) |
| `dbPing` | Verify connectivity |
| `dbQuery` / `dbExecute` | Forward SeaORM statement payloads (optional `txnId` from `dbBegin`) |
| `dbBegin` | Start a native engine transaction (or nested savepoint via `parentTxnId`); returns `txnId`. The host records a sticky per-task fault when this RPC fails so later statements cannot fall back to autocommit. D1 rejects interactive transactions and sets `interactiveTxn: false` on `dbConnect`. |
| `dbCommit` / `dbRollback` | Finish that transaction. A failed `dbCommit` is surfaced to `LibraryStore` (SeaORM's proxy hook is infallible); the guest is rolled back. |
| `dbAtomic` | Named library operation (claim redeem, last-owner guards, password hash, consume-once OIDC/WebAuthn) as one SQL transaction, with an `operationId` receipt for lost-response replay. D1 uses `{ "batch": [...] }` on the REST Query API. SQLite and Postgres run the same command in a native local transaction. |

Built-in ids: `sqlite`, `d1`, `postgres` (match `[database].plugin`).

## Examples

Native Echo:

```bash
cargo build -p bookclerk-plugin-echo-native-rust
mkdir -p "$BOOKCLERK_FILES_DIR/plugins/echo"
cp target/debug/bookclerk-plugin-echo-native-rust \
  "$BOOKCLERK_FILES_DIR/plugins/echo/"
cp examples/plugins-echo-native-rust/plugin.toml \
  "$BOOKCLERK_FILES_DIR/plugins/echo/"
```

Workerd Echo — install `plugin.toml` + `modules/` from any of
[`examples/plugins-echo-workerd-ts/`](../examples/plugins-echo-workerd-ts/),
[`plugins-echo-workerd-python/`](../examples/plugins-echo-workerd-python/),
[`plugins-echo-workerd-rust/`](../examples/plugins-echo-workerd-rust/), or
[`plugins-echo-workerd-fetch/`](../examples/plugins-echo-workerd-fetch/)
(outbound `*.example.com` + embedded logo; host spawns `bookclerk-workerd`,
not a SEA binary).

```toml
# config.toml
[integrations.echo]
enabled = true
```

```bash
bookclerk plugins approve echo --yes
bookclerk plugins list
bookclerk plugins enable echo
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

Users unpack under `plugins/` (or a `BOOKCLERK_PLUGIN_DIRS` root), **approve**
capabilities, then set `enabled = true` in `config.toml` (or
`bookclerk plugins enable`). No rebuild of Bookclerk is required when
`api_version` matches.

### First-party plugins (dual load via plugin host)

Audible, Libro.fm, Chirp, GraphicAudio, and Audiobookshelf ship as **external
plugins** under `crates/bookclerk-plugins/`. The host crate
`bookclerk-plugin-host` also registers the same adapters **in-process**
(`register_builtin_*` / `load_sources` / `load_integrations`) so `cargo run`
works without staging binaries. CLI/daemon call only those host helpers —
never store crates by name. Discovery skips an id that is already registered.

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
[plugin-registry.md](plugin-registry.md).
