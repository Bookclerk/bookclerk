# Windows AppContainer confinement notes

This page records enforcement details and measured assumptions for the
Windows spawn path. Green CI is necessary but not sufficient — each claim
below ties to code or an explicit limitation.

## Job Object ordering

Implemented in `bookclerk-sandbox` (`windows_launch.rs`), not rappct 0.13.3:

1. Default: `CREATE_SUSPENDED` → Job limits (`KILL_ON_JOB_CLOSE`, memory /
   active-process / optional CPU) → `AssignProcessToJobObject` → `ResumeThread`
   so no guest instruction runs before Job membership.
2. Optional: set `BOOKCLERK_AC_USE_JOB_LIST=1` to try
   `PROC_THREAD_ATTRIBUTE_JOB_LIST` first (with AppContainer caps + handle list).
   On hosts where that returns `ERROR_INVALID_HANDLE` (seen on GitHub
   `windows-latest`), CreateProcess retries the suspended path automatically.
3. On any failure after `CreateProcessW`, `TerminateProcess` the child and
   close process, thread, pipe, and Job handles.

Test hook: `BOOKCLERK_TEST_FAIL_JOB_ASSIGN=1` forces fail-closed teardown.

## Sibling native-behind-workerd

The host never nests an AppContainer. Microsoft documents that pipes created
inside a container must use `\\.\pipe\LOCAL\` and that pipes cannot be used
between different AppContainers (Win10 1709+). Creating a second container
from inside one also fails. So `bookclerk-workerd` cannot spawn or name-connect
the native backend.

Instead the unsandboxed host:

1. Creates two duplex links (guest stdio = two unidirectional pipes with the
   launcher end overlapped; proxy = one duplex overlapped pipe). Names are
   random. The DACL grants only the creating user, SYSTEM, and Administrators
   — not Everyone, Anonymous, or All Application Packages. Each pipe is the
   first instance, rejects remote clients, and is connected before the handle
   is delivered. The AppContainer guest uses that inherited handle; it does
   not open the pipe by name.
2. Spawns two `bookclerk-jail.exe` processes (gateway `OutboundListen`, guest
   `NetPolicy::Deny`). Isolation::Off still goes through the jail with
   `Enforcement::Disabled` so handle handoff has a single-threaded parent.
3. `DuplicateHandle`s each link end into the matching jail, then writes one
   bounded JSON handoff line on that jail's stdin
   (`BOOKCLERK_JAIL_HANDOFF=1`). The jail marks the duplicates inheritable,
   puts them on `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, and exports
   `handle:<n>` env. The list contains only those handed-off handles.
   Unrelated process handles stay off it. The host never marks a handle
   inheritable itself. A numeric handle value is not a session identity:
   the guest must write a per-session challenge on the proxy before the
   host serves mux frames.
4. Assigns both jail processes to one session Job (`KILL_ON_JOB_CLOSE` plus
   the grant's aggregate memory / active-process / CPU limits). Each jail's
   own Job still nests under it.

The guest never opens a named pipe for RPC or `SOCKET_PROXY`. OAuth callback
pipes stay host-created and ACLed to the **guest** Package SID. Isolation::Required
fails closed if either profile or the session Job cannot be created.

Permanent tests: E2 (inherited pipe + ambient TCP denied), E2b (two
AppContainers on one inherited duplex), E3 (same-container loopback for the
workerd bridge), E5 (closing the session Job kills the tree).

## Profile folder

`GetAppContainerFolderPath` is authoritative when the path is under Known Folder
LocalAppData `\Packages\`. Documented layout is
`%LOCALAPPDATA%\Packages\<moniker>\AC`; **measured on Windows CI** the API
returns `%LOCALAPPDATA%\Packages\<package-SID>`. Bookclerk then ensures the
`\AC` child exists and uses that as cwd / `LOCALAPPDATA` (`TEMP`/`TMP` →
`AC\Temp`). Fail closed on API failure or paths outside Packages — Bookclerk
does **not** synthesize a Packages path when the API fails.

## Cross-process ACL sync

Named mutex `Local\bookclerk-dacl-tx` (120s timeout, fail closed) around every
DACL RMW, plus an in-process mutex. The same pair serializes
`CreateAppContainerProfile` and `DeleteAppContainerProfile`: those Win32 calls
are not thread-safe, and four overlapping session launches plus each jail's
`attach` otherwise sit inside the API together. The lock is released before
the host waits for jail-ready, so the jail can attach while the host is
waiting. Ancestor traverse ACEs are written with
`SetKernelObjectSecurity` so inheritable ACEs already on a broad parent
(`%TEMP%`, a build directory) are not propagated to every child while that
mutex is held. Leaf directory grants still use inheritable ACEs.
Revoke does not invalidate already-open handles.

## Interactive listen / OAuth callbacks

Capability SIDs and same-host loopback are **different knobs**.
[`internetClientServer` / `privateNetworkClientServer`](https://learn.microsoft.com/en-us/windows/uwp/networking/networking-basics)
authorize **remote** inbound (Internet / private LAN), not host↔AppContainer
localhost IPC. Same-machine host→guest TCP on loopback requires a
[`CheckNetIsolation LoopbackExempt`](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/troubleshooting-uwp-firewall)
escape hatch that Microsoft documents as **dev-only** — and measured for
Bookclerk’s unpackaged `CreateAppContainerProfile` guests, even an active
`-a/-is` session did not restore host→guest connect.

There is no AppContainer capability SID for “talk to localhost.” Product code
must **not** call CheckNetIsolation or ship a LoopbackExempt dependency.

For Bookclerk: the host owns the browser TCP listener and proxies bytes to the
guest over IPC (`callback_proxy` + `callback_ipc` on `login.start`). The Windows
named pipe is ACLed to that guest’s Package SID (`GRGW`) with a Low mandatory
label (`S:(ML;;NW;;;LW)`). See [plugins.md](plugins.md) (Interactive listeners).

## Availability

| | Plugins | Media |
| --- | --- | --- |
| Job memory (cumulative) | 512 MiB | 2 GiB |
| Active processes | overhead + extra (direct-native 1+2=3, isolate 2+2=4, native-behind gateway 2, guest 1+extra, outer session Job 5+extra) | 64 |
| CPU rate | 80% of one core hard cap on the outer session Job (sibling inner specs send `"off"` so the label default is not applied; standalone Jobs still set a percent) | uncapped |
| Stderr proxy budget | 1 MiB | 16 MiB |
| data/tmp growth (plan + side-pass) | 512 MiB each | n/a |
| RPC timeout | kill + quarantine | n/a (stdio job) |

Defaults come from the jail label (`plugin:…` vs `media-…`) and grant/host
ceilings. A `Spec` may set `memory_bytes` / `active_processes` /
`cpu_rate_percent` explicitly. Omitted or null CPU is unspecified and takes the
label default; a number is a percent; `"off"` disables the rate and is not
filled. Memory uses Job Object **job-wide** commit charge
(`JOB_OBJECT_LIMIT_JOB_MEMORY`), aligned with Linux cgroup `memory.max` (main +
children). Limits are best-effort Job Object + host policy, not a hard
multi-tenant quota.

Cross-platform: Linux applies the same Spec fields via cgroup v2 into an
**exclusive** child cgroup when the hierarchy allows it (never onto a shared
parent slice, and never a leaf that already exists). The leaf stays inside
the process's current cgroup. Bookclerk does not place it on an ancestor.
When that cgroup has member processes, the host process moves into
`bookclerk-host` and the leaf is created beside it, still under the same
parent, so that parent's limits and `cgroup.kill` still cover the session.
`pids.max` counts threads.
It is the infrastructure thread budget plus `extraProcesses`, not the Windows
outer baseline (5 + extra). `bookclerk-workerd` pins its Tokio worker and
blocking-thread counts so that budget does not grow with CPU count. If a leaf
cannot be created, the
resources layer is reported not-applicable, process-group kill is the fallback
(it does not cover `setsid`), and Required still rests on FS/net. When the
leaf exists, teardown is `cgroup.kill` and a wait for `populated 0`. That
owns the leaf, including a `setsid` descendant. A missing `cgroup.kill` is
unsupported; Bookclerk does not signal pids from a membership snapshot. It is
a different path from process-group kill. macOS
Seatbelt cannot enforce memory/CPU/pids — Bookclerk reports that layer as not
applicable and does not fake enforcement.

The host keeps a journal of each session's package SID and the paths that
session's spec grants: inheritable leaf ACEs, no-inherit ancestor traverse, and
the profile folder plus `Temp`. After the session Job is closed and the
siblings have been reaped, on graceful exit and on failed startup, the host
deletes the AppContainer profiles and then revokes that journal under
`Local\bookclerk-dacl-tx`, and only then removes the session directory.
`DeleteAppContainerProfile` does not strip package-SID ACEs, and the profile
moniker (`bc.<stem>.<hex>`) is not the SID. Directory removal is the signal
that those ACEs are gone. Revoke is idempotent, treats a missing path as
success, and removes only that session's SID. The jail may revoke as well when
its process exits normally. Job kill skips that `Drop`, so the host journal is
the owner that still runs. Ancestor traverse stays a `SetKernelObjectSecurity`
write on that directory alone.

## AppContainer vs LPAC

Bookclerk uses regular AppContainer (capability SIDs + path ACLs + Job). Less
Privileged AppContainer (LPAC) ambient restrictions are not applied.

## Trust

Sandboxing ≠ publisher authentication. SHA-256 verifies artifact integrity, not
publisher identity. Install UX should surface registry/publisher/network (and
callback-transport if ever shipped) / signature before unattended install;
unsigned installs need explicit approval. See [plugins.md](plugins.md).

## Upstream

rappct 0.13.3 assigns the Job after the guest may already run and does not
resume suspended threads or reliably terminate on mid-launch failure. Bookclerk
bypasses `launch_in_container_with_io` for production launches and keeps rappct
for profile/SID/capability helpers only. File/track an upstream issue on
[cpjet64/rappct](https://github.com/cpjet64/rappct) describing:

1. `CreateProcess` then `AssignProcessToJobObject` race with a runnable primary thread
2. no `ResumeThread` when `suspended: true`
3. failure after create does not reliably `TerminateProcess`
