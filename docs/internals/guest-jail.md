# The guest jail

How Bookclerk confines every plugin: why plugins run as separate processes, the trust model, the `bookclerk-jail` launcher and the per-platform sandboxes. Operators turn isolation on and off in [Managing plugins](../server/plugins.md#isolation-modes); authors declare what a plugin needs in [Capabilities and consent](../plugins/capabilities.md).

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
| No library DB path (sources / integrations / outputs) | `library.db` is never passed on the wire to those kinds — and not reachable if it were. **Database** sqlite guests open the file at a jail-granted path (`BOOKCLERK_SQLITE_PATH` / `sqlitePath` at session open) |
| No files-dir root | Plugins get `plugin_data_dir` (`$FILES_DIR/plugin-state/<PluginKey fs-id>/data`) and a per-fetch work directory under their own `TMPDIR` — not `master.key` or the download cache root |
| Env scrub | Child spawn uses `env_clear` + a small allowlist (`PATH`, locale, …). `BOOKCLERK_*`, `AWS_*`, tokens, and DB URLs are not inherited; `HOME` and `TMPDIR` are replaced with the guest's own directories |
| Host-mediated secrets | `login` returns `{ account, credentials }`; host seals into `encrypted_secrets` with `provider = plugin id`. `scan` and `fetchTitle` receive those blobs from the host |
| Host-mediated library writes | `scan` returns book DTOs; host upserts with `source` forced to the plugin id. Account listing for a plugin id is answered from the host accounts table |
| Scoped identity | Plugin cannot claim another storefront’s `source` / `provider` |
| Network consent | Operator runs `bookclerk plugins approve` before enable; the **same covering grant is required again at every external spawn** and at privileged delivery (`config` / `secrets` / `work_fs` / `oauth`). **Every redirect hop** is checked (not only the initial host). Resolved IPs are checked against private/local/metadata ranges; Host/SNI mismatch and DNS rebinding are rejected (broker IP/SNI enforcement is follow-up — do not treat initial-host-only as the frozen contract). Brokered HTTP uses the same domain grants; **raw TCP, UDP, and listen are distinct capabilities**. Native outbound is jail default-deny, not permanently coarse-unrestricted |

First-party guests ship under `crates/bookclerk-plugins/` with the guest SDK
contract. Host binaries (`bookclerk`, `bookclerkd`) depend on
**`bookclerk-plugin-host`** only — not on individual store crates. Release
builds and local `cargo dev` both load staged guests from `plugins/`.
Discovered copies of the same **PluginKey** are skipped. Duplicate display
aliases from different PluginKeys on the **same host** are invalid
installation state (discovery fails closed). Instances, deployments, schemas,
and accounts are keyed by PluginKey in the database; only the database
adapter (`[database].plugin`) still occupies a `config.toml` key, and it
upgrades a unique alias to its PluginKey when `bookclerk plugins deploy`
or a settings save touches it. A bare alias is accepted only when exactly
one install on this host uses it.
After registration, hosts talk **only**
through `ContentSource` /
`Integration` (login, scan, fetch, import, revoke, inspect, plus catalog
`searchCatalog` / `catalogDetail` / `expandCandidates` / `purchaseHint` /
`listDeals` for Discover). Sources always return `PlainFetch` (`SourceFetch` is
an alias) —
DRM (Adrm/CENC) is decrypted inside the Audible plugin before the host sees
media. Guest `fetchTitle` carries optional `pdfUrl`; catalog methods are on the
Workers RPC wire for external guests. When an account is removed (Accounts
page or `bookclerk library revoke-account`), the host first calls the guest's
`revokeAccount` with the account's stored credentials so it can sign out at
the store (Audible deregisters the device it registered at login), then
deletes the credentials whatever the guest answered. The SDK default does
nothing.

Enabling a third-party plugin still means running that guest as the Bookclerk
user, inside the jail below — review and **approve** capabilities before
enabling.

## Shipping without a store

Both hosts **link no storefront**. Storefronts are staged guests only:

```bash
# Default: external guests only (release packaging).
cargo build --release -p bookclerk-cli -p bookclerkd
```

This exists for Audible specifically. Adrm and Widevine CENC decrypt live in that
plugin, and some regions restrict distributing a binary that can circumvent
DRM — so whoever packages Bookclerk needs the option to ship hosts that contain
no such code at all, rather than a build flag that merely disables it at runtime.
Omitting the guest omits the crate, and with it the ciphers, the content-key
handling, and the CDM.

Nothing else has to move for that to hold. The shared MP4 plumbing
(`bookclerk-mp4`) parses and rewrites containers and takes a `SampleTransform`
from its caller; the Audible plugin's transform is the only one that decrypts.
`scripts/check-store-free-hosts.sh` and `scripts/check-plugin-architecture.sh`
assert it in CI: default hosts must link no plugin package and reach no cipher
crate, and must not grow an in-process `bundled-plugins` path. A shared crate
that grew an `aes` dependency fails the default-host check.

Users of a store-free build can still add any storefront back as an external
guest, since discovery is independent of these features. That is a deployment
choice made by whoever installs the plugin, not by whoever shipped the binary.

## How a guest is confined

Every external guest is started by **`bookclerk-jail`**, a small launcher that
applies a confinement policy to itself and then `exec`s the program the host
named. **`bookclerk-workerd`** is the front door: for `runtime = "workerd"` it
loads the author's isolate; for `runtime = "native"` the host also starts the
native backend as a **sibling** jail. Guest RPC is a host-created inherited
link (`BOOKCLERK_GATEWAY_GUEST_RPC` on the gateway; the guest uses stdin and
stdout). The CONNECT mux (`BOOKCLERK_SOCKET_PROXY` on the guest) is served by
the **unsandboxed host**. An AppContainer cannot dial the host's loopback, so
the jailed gateway must not be the process that connects granted TCP, and
Bookclerk does not add a machine-wide loopback exemption. Unjailed conformance
spawns may still hand that mux to `bookclerk-workerd`
(`BOOKCLERK_GATEWAY_PROXY`). On Unix each link is one duplex socket
(`fd:<n>`). On Windows each link is two unidirectional pipes so a concurrent
read cannot lock the write (`BOOKCLERK_GATEWAY_GUEST_RPC` is the gateway read
half and `BOOKCLERK_GATEWAY_GUEST_RPC_WRITE` is the write half; the guest
proxy write half is `BOOKCLERK_SOCKET_PROXY_WRITE`). `bookclerk-workerd` never
nests a second jail.

Nesting is impossible on two of the three production OSes. A Seatbelt
process cannot apply a second profile (`sandbox_init` returns EPERM even
when the outer profile is `(allow default)`). A Windows AppContainer cannot
create a second container, and named pipes do not connect across different
AppContainers (Win10 1709+). Linux Landlock domains can stack, which is why
the old nested topology appeared to work there. The host therefore applies
each jail once from an unsandboxed parent on every OS.

What each jail grants is decided entirely by the host. The gateway jail is
`OutboundListen` (loopback RPC bridge to the pinned `workerd`); it may write
only the host-owned `plugin-state/<PluginKey fs-id>/session-<nonce>/`
directory (`0700`, also `BOOKCLERK_WORKERD_STATE_DIR` and the gateway's
`TMPDIR`). The native sibling is `NetPolicy::Deny` and must use the SDK
socket proxy over the inherited link. The guest's `TMPDIR` stays its own
`tmp/` scratch, so it cannot read gateway state. Workerd is not a substitute
for the jail.

Direct host↔native Cap'n Proto (no `bookclerk-workerd` in between) exists only
as `SpawnTransport::DirectNativeDiagnostic` on `SessionServices`, used by jail
probe tests and the transport latency benchmark; `bookclerkd` and `bookclerk`
never select it. A missing `bookclerk-workerd` or pinned `workerd` is a hard
spawn error in **every** isolation mode (`refusing to start plugin … the
bookclerk-workerd front door … is unavailable`); nothing falls back to direct
native.

A native guest gets three paths and nothing else:

| Path | Access | Also known to the guest as |
| --- | --- | --- |
| its install directory | read-only | `cwd` |
| `…/plugin-state/<PluginKey fs-id>/data` | read/write | `HOME`, and `plugin_data_dir` on the wire |
| `…/plugin-state/<PluginKey fs-id>/tmp` | read/write | `TMPDIR` / `TEMP` / `TMP`; fetch scratch is `tmp/fetch` |

The host-owned `session-<nonce>/` directory is **not** on that list. It is
the gateway's `TMPDIR` / `BOOKCLERK_WORKERD_STATE_DIR` (granted sockets live
there). The guest jail has no read of it. The vat removes the directory when
the session drops.

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

### Why fetch scratch lives under plugin `tmp` rather than the cache root

A guest is long-lived — one process per plugin, serving every call for the life
of the daemon — and filesystem confinement is fixed at spawn. Granting the whole
download cache would let one plugin read or overwrite every other fetch's
scratch.

The host therefore never grants the cache root. `fetchTitle` receives a
`cache_dir` under the guest's already-granted `TMPDIR` (`plugin-state/<PluginKey fs-id>/tmp/fetch`).
Returned media paths are under that directory; the unconfined host reads them
afterward. Destinations ingest **byte streams** over the plugin ABI rather than a
host file descriptor. SQLite gets spawn-time file grants for `library.db` and
its journal sidecars (never the files-dir parent).

v1 passed a per-call directory over a Unix socket with `SCM_RIGHTS` on fd 3.
The product ABI does not arm that channel: workerd cannot `recvmsg`, and a live
`BOOKCLERK_PLUGIN_FD_CHANNEL` with no matching send deadlocks the guest. A
native SCM_RIGHTS shortcut remains a possible host-selected optimization behind
streams, not the public contract.

A media job is confined far more tightly: one input file, one output directory,
per job, in a process that exits when the job does. See [media.md](../server/media.md).

### Why a launcher and not self-confinement

On Linux and macOS `bookclerk-media-worker` confines itself, which is fine
because that binary is ours (on Windows, which cannot confine a running
process, `bookclerk-jail` confines it at spawn instead): only our own code could skip the call. A plugin guest inverts the
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
applies the policy — except the host-listed `preserve_fds` (gateway 3 and 4,
guest 3) that carry the inherited RPC and proxy links — and refuses to hand
over at all if it cannot enumerate them. Nothing else leaks today, because
Rust opens files `O_CLOEXEC`; the point is that this stops being a property
of every library the host links, re-checked on every dependency bump, and
becomes a property of the jail.

## Windows confinement

Windows cannot confine a process after it has started. `bookclerk-jail` therefore
`CreateProcess`es the guest into an AppContainer (via
`bookclerk_sandbox::spawn::run_appcontainer`), ACLs the policy's **explicit**
read/write paths for a **per-launch** Package SID, maps `NetPolicy` to
capability names (`internetClient`, `privateNetworkClientServer`, …), places the
guest in a kill-on-close Job Object, and proxies stdio until the guest exits.

Native-behind-workerd guests get a **second** AppContainer (`NetPolicy::Deny`,
no `internetClient` SIDs), spawned by the host as a sibling of the gateway
container. IPC is an inherited duplex (Windows handle handoff through
`bookclerk-jail`); the guest never opens a named pipe. OAuth callback pipes
use the guest Package SID. Isolation::Required fails closed if either
profile cannot be created. Windows `Isolation::Off` still requires
`bookclerk-jail.exe` beside the host so handle handoff can run with
`Enforcement::Disabled`. A host-owned session Job (`KILL_ON_JOB_CLOSE`)
covers both jail processes.

### Job Object launch ordering

Bookclerk owns CreateProcess (not rappct’s assign-after-run path). When Job
membership is required, the launcher prefers `PROC_THREAD_ATTRIBUTE_JOB_LIST`
so assignment happens before any guest instruction runs; otherwise it uses
`CREATE_SUSPENDED` → configure Job (`KILL_ON_JOB_CLOSE` + resource limits) →
`AssignProcessToJobObject` → `ResumeThread`. Any failure after CreateProcess
terminates the child and closes process, thread, pipe, and Job handles.
Descendants cannot outlive jail/profile/ACL cleanup.

### Profile paths

`GetAppContainerFolderPath` is authoritative when under Known Folder
LocalAppData `\Packages\`. Docs describe `Packages\<moniker>\AC`; Windows CI
measures `Packages\<package-SID>` (with `\AC` used when that child exists).
Bookclerk fails closed if the API fails or returns a path outside Packages — it
does **not** synthesize a Packages path. Child cwd / `LOCALAPPDATA` use that
folder; `TEMP`/`TMP` use `<folder>\Temp`.

### ACL mutations

Bookclerk **never mutates DACLs under protected OS-managed roots** (Windows /
System32 / WinSxS / Program Files / ProgramData\Microsoft, resolved via
`GetWindowsDirectoryW` / `GetSystemDirectoryW` / Known Folders). Ambient OS
runtime access (loading system DLLs / `cmd.exe`) comes from existing OS ACLs
such as ALL APPLICATION PACKAGES. Explicit policy paths are temporarily ACLed
for the Package SID. Cross-process DACL read/modify/write is serialized with the
named mutex `Local\bookclerk-dacl-tx` (plus an in-process lock; 120s fail-closed
wait). Ancestor traverse ACEs (`FILE_TRAVERSE`, no inheritance) are written
with `SetKernelObjectSecurity` so Windows does not propagate inheritable ACEs
through large parents such as the user temp directory or a build tree while
the mutex is held. Leaf directory grants still inherit onto their children.
Revoking an ACE does **not** invalidate handles the guest already opened.

Plugin hosts create the AppContainer profile up front, put
`windows_profile_name` on the jail `Spec`, and delete the profile when the
plugin client drops. Native-behind-workerd sessions create **two** profiles
(gateway + guest) and one session Job that both `bookclerk-jail.exe`
processes join. The host starts the guest jail only after the gateway jail
has `CreateProcess`ed its child, so the two launches do not hold
`Local\bookclerk-dacl-tx` at the same time. Media jobs leave that field unset
so the jail creates a unique profile per job.

Fetch scratch and plugin state are the spawn-time `data` / `tmp` grants (already
ACLed for the Package SID). SQLite adds file-level ACLs for `library.db` and
journal sidecars at confine time. Destinations stream bytes and do not receive
host cache paths. The host does not apply a per-RPC extra path ACL for fetch/upload.

### Interactive listeners (OAuth and similar)

`capabilities.bindings.oauth = true` declares that the storefront needs an
OAuth-style callback. The **host** owns the browser-facing TCP listener and
forwards each connection over a duplex IPC tunnel (Unix socket under the plugin
scratch dir, or a Windows named pipe) into the guest, which still runs its HTTP
stack (Audible LoginServer). `loginStart` carries `callback_ipc` +
`callback_public_base`; the guest must not bind TCP when those are set.

On Windows the named pipe is created with a Package-SID DACL (plus SYSTEM /
Administrators / Creator Owner) and a Low mandatory integrity label so the
AppContainer guest can open it; remote clients are rejected. On Unix the host
creates one mode `0700` directory, short enough that `{dir}/.s.PGSQL.<port>`
fits in a macOS `sockaddr_un` (104-byte `sun_path`). That directory is the
guest's only pathname-socket grant and a write; `/tmp` itself and the gateway
session directory are not. Each OAuth callback proxy binds its own socket
file in that directory. The name is shorter than `.s.PGSQL.65535`, and an
occupied name is left in place. Linux Postgres still binds through
`/proc/self/fd`; macOS Postgres binds in the same directory. A path that
does not fit fails with the length and the directory that was too long.

This is required on Windows AppContainer (host↔guest loopback is blocked even
with Full caps / CheckNetIsolation) and is used on all OSes for a uniform
contract. Do not enable CheckNetIsolation. `--external` paste flows remain
available as a fallback.

### Availability

Plugin Jobs get conservative memory / active-process (and optional CPU) limits;
media workers get higher defaults. When a jail `Spec` carries
`memory_bytes` / `active_processes` / `cpu_rate_percent` (percent of **one
logical CPU**; values above 100 request multi-core bandwidth up to
`logical_cpus × 100`), those values override the label heuristics on Windows
(Job `CpuRate` is scaled by core count so the meaning matches Linux cgroup
`cpu.max`; Job memory is **job-wide** commit charge, matching Linux
`memory.max`). Operator grants expose an **extra** process/thread budget
(`extraProcesses`, default **2**) above launcher overhead (direct-native **1**,
workerd isolate **2**, native-behind payload **3** split as gateway **2** +
guest **1 + extra**). Spec `active_processes` = overhead + extra (capped at
64). The Windows **outer** session Job counts both `bookclerk-jail`
supervisors as well, so its cap is **5 + extra** (still capped at 64). That
outer Job is the only sibling Job that sets the CPU hard cap. Gateway and
guest specs set CPU to `"off"` so a nested Job is not filled with the plugin
label default (80). Omitted or null stays unspecified and still takes that
default; a JSON number is a percent. A standalone jail (no outer Job) still
sets CPU on its own Job.
Linux `pids.max` is a thread budget and is **not** given this Windows process
count. Creating the outer Job under `isolation = "required"` fails the spawn
before either sibling starts. `best-effort` continues without an outer Job
only when the kernel reports that Job control as unsupported, and the session
records that absence.
Workerd consent does not edit process budget (host-managed headroom). Workerd
guests use isolate `cpu_ms` for the script budget; their jail CPU rate comes
from the host default / `[plugins.jail]` per-jail ceiling (default **80**)
rather than a per-plugin `cpuRatePercent`. On Linux the Spec fields are applied
best-effort via an exclusive cgroup v2 leaf (never written onto a shared parent
slice). The leaf name includes the plugin id, the host pid, and a per-session
nonce; creating a leaf that already exists fails, and two sessions of the same
plugin do not share one. `pids.max` is the infrastructure thread budget
(pinned `bookclerk-workerd` workers and blocking threads, the pinned
`workerd` process, the guest runtime, and the jail supervisors) plus the
guest's `extraProcesses` allowance. It is not the Windows outer process count
(5 + extra) and not the payload process count (3 + extra). `bookclerk-workerd`
pins `worker_threads` and `max_blocking_threads`, so that budget does not grow
with the host CPU count. A cgroup that already contains the host cannot
enable domain controllers for its children. The session leaf stays inside the
process's current cgroup. Bookclerk does not walk to an ancestor or enable
controllers on one. When that cgroup already delegates `pids` and has no
member processes, the leaf is created there. When it has member processes,
Bookclerk creates `bookclerk-host`, moves only this process and its
descendants that are still in the cgroup, enables controllers on that cgroup,
and creates the leaf beside `bookclerk-host`. A process that is not a
descendant is not moved; the moves are rolled back and the leaf is not
created. A later session whose current cgroup is `bookclerk-host` creates the
leaf as a sibling when the immediate parent already delegates `pids`, and
does not inspect the grandparent. A controller file that is not present is
skipped; `pids.max` is
required, and a leaf without it is not kept. Teardown writes `cgroup.kill`
and waits until `cgroup.events` reports `populated 0` before removing the
leaf. That is kernel ownership of the leaf: tasks that fork during the kill,
and descendants that have called `setsid`, die with the leaf. If `cgroup.kill`
is missing, teardown returns an error that says equivalent teardown is
unsupported. Bookclerk does not signal pids read from `cgroup.procs`. Creating that
leaf or writing `pids.max` is often refused inside desktop app
cgroup scopes (browsers, IDEs). Bookclerk reports that and falls back to
process-group SIGKILL. That fallback is a different path: a descendant that
has called `setsid` is outside the group, and without a delegated cgroup
this path does not own it. The host
records each leader's pid and start time (`/proc/<pid>/stat` starttime on
Linux, `proc_pidinfo` on macOS). It observes leader exit with `waitid`
`WNOWAIT` so the zombie still has that start time, signals the group only
while the start time matches, and reaps afterward. It does not signal a
process-group id whose start time is gone.
Tokio `Child::wait` is cancellation-safe; cancelling it does not drop zombie
status. On macOS Seatbelt the resource fields are ignored (documented as
unsupported — FS/net only). `[plugins.jail].cpu_rate_percent` is a **per-jail ceiling** only (not a
cumulative reservation; default 80). Quotas cap how fast a guest may burn CPU;
if many plugins’ ceilings sum above host capacity, the OS scheduler shares
cycles among runnable guests. Each plugin's `data/` and `tmp/` directories are
capped at **512 MiB each**: the host measures them at jail plan (spawn/reload).
Over budget refuses the spawn. RPC timeouts and framing violations
likewise kill and quarantine. Stdin proxying does not block jail exit after the
guest terminates.

### Trust vs sandbox

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
