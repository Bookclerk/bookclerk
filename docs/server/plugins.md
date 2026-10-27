# Managing plugins

This page is for the people who run a Bookclerk server. It covers how
plugins are installed, approved, enabled, configured and placed on the
hosts of a library, where they live on disk, and how isolation is
configured.

What a plugin may ask for is in
[Capabilities and consent](../plugins/capabilities.md). How the jail works
is in [The guest jail](../internals/guest-jail.md).

## Installing plugins

A third-party plugin is **installed** into the cluster once. A package
version is resolved and verified once, recorded in the library database,
and run byte-for-byte the same on every node.

Installing doesn't touch this node's `plugins/`. **Deploying** decides
which nodes run it (see [Placement](#placement)).

```sh
bookclerk plugins install cargo:bookclerk-plugin-source-example@1.2.0 --approve-capabilities
bookclerk plugins install example --archive ./example.tar.gz \
    --manifest ./bookclerk-package.json --allow-unverified-publisher
bookclerk plugins versions [example]         # every installed version; * marks the active one
bookclerk plugins update example [--to 1.3]  # install a newer registry version and activate it
bookclerk plugins rollback example 1.2.0     # activate an earlier installed version
bookclerk plugins approve example --yes      # record consent on the active version
bookclerk plugins deploy example             # run the default instance on this node
bookclerk plugins uninstall example          # clear the active version; undeploy everywhere
```

### What installing checks

1. **Trust.** Installing checks the trust policy.
2. **Artifacts.** It fetches each artifact and verifies it. A registry
   artifact must match the archive digest its package manifest declares.
   A local archive's digest is computed from the operator's own file.
3. **Contents.** The node extracts the archive to scratch, parses
   `plugin.toml`, checks that the file the package manifest names as
   `executable` is present, and computes the payload-root digest.
4. **Record.** The version row records the PluginKey, the version from
   `plugin.toml`, the manifest and settings schema, one artifact per
   target (URL, archive digest, payload-root digest), the coordinate it
   came from, and the consent grant.

Installing the same version twice does nothing. Installing different bytes
under an installed version is refused.

A local archive's path names no stable plugin, so a later archive of an
alias that is already installed reuses that alias's key. One alias names
one plugin: a version may not take an alias another plugin's active
version answers to.

### Artifact URLs

- A registry artifact keeps its download URL.
- A local archive up to 1 MiB is stored in the database
  (`blob:sha256:<hex>`), so every node can materialize it.
- A larger local archive is referenced in place as a `file://` URL. Only
  nodes that see that path (shared storage) can materialize it; the others
  record an `error` observation.

A host node of another cluster fetches an owner's `https:` artifact only
when both are true:

- its host operator approved that exact URL, archive digest and payload
  digest (they are part of the reviewed request);
- neither its host nor any redirect hop's host is, or resolves to, a local
  or private address. This is checked on the addresses actually dialed,
  with no proxy.

See [Host consent](../adr/control-plane.md#host-consent).

### The active version

Each PluginKey has at most one active version.

- The first installed version becomes active. A later one becomes active
  with `install --activate`, `update` or `rollback`.
- An automatic install (an operator package, or `install` without
  `--activate`) never moves the active version back to an older one.
- Changing it publishes `install.changed` to every node queue, and each
  node respawns its deployments of that plugin from the new digest.

### Consent rides on the version

A version with no approved grant is recorded but not spawned. Nodes report
an `error` observation until it is approved.

A new version carries the previous active version's grant forward when
that grant still covers everything the new version requests. A version
that asks for more needs a fresh `approve`.

### Materialization

When a present deployment needs the active version, the node:

1. Fetches the artifact for its own target.
2. Extracts it under `plugins/.staging/version-<digest>.<pid>`.
3. Verifies the payload-root digest against the row.
4. Renames the tree to `plugins/.versions/<digest>`.

A partial extract never reaches the final path, and a cached digest is
immutable. Digests no deployment on the node references are swept after
an hour.

### Health is an observation

A guest that fails health records an `error` observation, and the version
stays active. Nothing rolls back automatically: use `plugins rollback` to
activate an earlier version.

### Staged packages

Platform guests (`local`, `sqlite`) ship with the node, and
`cargo stage-plugins` stages development trees under `plugins/<fs-id>`.
Those are staged on one machine, not installed into the cluster. Their
provenance is `platform_bundled`, `verified_staged` or
`local_development`, while an installed version is `installed`.

### Install API

The daemon offers the same operations to an operator session:

| Method | Path | Body / result |
| --- | --- | --- |
| `GET` | `/api/config/plugins` | Every installed plugin: `active_version` and its `versions` (each with `active`, `approved`, and artifacts) |
| `POST` | `/api/config/plugins` | Install: `{coordinate}` or `{package}` (a directory under `plugin-packages/`), plus `activate`, `approve`, `allow_unverified_publisher` |
| `GET` | `/api/config/plugins/{key}` | One plugin with its versions and pinned settings `schema`; 404 when not installed |
| `DELETE` | `/api/config/plugins/{key}` | Uninstall: clear the active version, drop placements, undeploy everywhere |
| `PUT` | `/api/config/plugins/{key}/active` | `{version}`: update or roll back |
| `POST` | `/api/config/plugins/{key}/versions/{version}/approve` | Record the requested consent |

`{key}` is the percent-encoded PluginKey. A conflict with an installed
version or alias is `409`, and an unknown plugin or version is `404`.

## Enabling and settings (database)

Plugin configuration has one home: the **plugin instance document** in
the library database.

> [!NOTE]
> There is no `[sources.<id>]`, `[output.<id>]` or `[integrations.<id>]`
> table, and no `BOOKCLERK_SOURCE_<ID>_ENABLED` override. Only
> `[database]` (which database to open) stays in `config.toml`.

### Instances

Every discovered plugin, except database adapters, gets a **default
instance** at enrollment, with a document seeded from the manifest's
`[[settings]]` defaults.

- Operators may create more instances of one plugin
  (`bookclerk plugins instances create <plugin> --name …`).
- A bare plugin key or alias routes to the default instance, and jobs may
  pin another with `--instance`.

### Deployments

**Enabled on a node** means the instance's deployment row for that node
says to run it. Each row's `desired` state also says whose decision it
is:

| `desired` | Written by | The node |
| --- | --- | --- |
| `present` | An operator (`plugins deploy`, the Settings switch) | Runs it |
| `absent` | An operator (`plugins undeploy`) | Retires it |
| `placed` | The cluster scheduler ([placement](#placement)) | Runs it |
| `unplaced` | The cluster scheduler | Retires it |

Only an operator writes `present` and `absent`, and only the scheduler
writes `placed` and `unplaced`. The library refuses anything else.

The reconciler spawns rows that run, retires the guest when a row stops
running, and records observations (`materialized`, `running`, `healthy`,
`error`, `retired`).

- Third-party plugins enroll `absent` until an operator approves their
  grant and enables them.
- Platform guests (`local`, `sqlite`) grant themselves (the reconciler
  persists that grant before the first spawn) and come up enabled.

```sh
bookclerk plugins list                                  # instances + deployments
bookclerk plugins approve echo --yes                    # consent (third-party)
bookclerk plugins deploy echo                           # desired = present on this host
bookclerk plugins deploy echo --node <node-id>          # … on another cluster host
bookclerk plugins undeploy echo
bookclerk plugins instances list --plugin s3
bookclerk plugins instances create s3 --name "Backup bucket"
bookclerk plugins instances create local --name "NAS disk" --node <node-id>   # node-local
bookclerk plugins config get s3                         # defaults under the document
bookclerk plugins config set s3 bucket my-audiobooks
bookclerk plugins config set s3 secret_access_key --secret   # value read from stdin
bookclerk plugins config unset s3 prefix                # back to the schema default
```

The daemon offers the same operations to an operator session:

| Method | Path |
| --- | --- |
| `GET` / `POST` | `/api/config/plugin-instances` (list; create `{plugin_key, display_name, node_id?}`) |
| `DELETE` | `/api/config/plugin-instances/{id}` (no deployments left) |
| `GET` / `PUT` | `/api/config/plugin-instances/{id}/config` (`{expected_revision, settings, secret_refs, secrets}`; `secrets` are sealed server-side) |
| `PUT` | `/api/config/plugin-instances/{id}/deployments/{node_id}` (`{desired, expected_revision}`) |
| `GET` | `/api/config/nodes`; `/api/config/plugins/{key}` carries the pinned schema |

### Settings → Plugins

**Settings → Plugins** shows each instance from its pinned schema:

- The Deployed switch writes this node's deployment (after consent, when
  needed).
- Secret fields are write-only.
- Save writes the document with compare-and-swap.

`PATCH /api/settings` no longer edits plugin keys (see
[Configuration](configuration.md) for the keys it takes).

### The pinned schema

Writes are checked against the schema pinned in `plugin_schemas` at
enrollment. So a plugin can be configured from any host in the cluster,
including hosts where it isn't installed.

The pin carries the package version. The newest installed version wins,
and every enrollment pass reconciles documents to its schema. A host still
running an older package refuses to spawn the plugin until it is upgraded
(see [`[[settings]]`](../plugins/manifest.md#settings)).

A spawn checks the document against the host's local manifest. Unknown
keys are dropped with a warning, and a missing `required` key is an
`error` observation.

### What the reconciler runs

The reconciler runs a plugin's active
[installed version](#installing-plugins) when it has one, materializing it
by digest. It falls back to a host-local tree only for a plugin with no
installed version.

An operator package under `$BOOKCLERK_FILES_DIR/plugin-packages/<name>/`
(`package.json` plus `archive.tar.gz`, with artifact URLs limited to files
inside that directory) is published as an installed version the first
time a present deployment needs it. That happens offline, with the key's
existing `plugin-grants.json` approval carried onto it.

- The reconciler respawns a guest that has exited, even when the last
  observation was healthy.
- The daemon keeps one guest map for the process.
- A config reload enrolls the node again and leaves running guests in
  place, unless the database connection changed. Then every guest is
  retired, and the reconciler brings the present deployments back.

### How a node hears about a change

Each node consumes its own queue, `node.<node_id>`, on the outbox.

- Every committed deployment write (create, desired-state change, delete)
  publishes a `deployment.changed` message to the deployment's node.
- Every committed configuration document (cluster settings and instance
  documents) publishes `configuration.changed` to every registered node.
- The daemon's delivery worker claims those messages under the catalog id
  `node:<node_id>`, and wakes the deployment and configuration
  reconcilers.

There is no fixed 5-second re-read any more, only a 60-second backstop
pass for a wakeup sent while the node's catalog row wasn't live. Messages
are wakeups, not state: the reconciler always re-reads the authoritative
rows.

The CLI only produces them (`bookclerk plugins deploy --node <id>` wakes
that node). `bookclerk queues list` shows each node queue with its
`node:<id>` consumer.

### Instances and jobs

A deployed guest is registered under its plugin instance id.
`bookclerk library scan` and `bookclerk library acquire` pin their queued
jobs to a named plugin key's default instance. Pass `--instance` to pin
another.

Accounts are keyed by plugin key, not instance. Every instance of a
plugin, on every host, serves the same connected accounts.

## Placement

By default an operator deploys an instance node by node with
`plugins deploy --node`. **Placement** hands that job to the cluster
scheduler:

1. The operator says how many nodes should run the instance, and which
   nodes may.
2. The scheduler picks the nodes with the most room, and writes the
   per-node deployments.

Nodes never connect to each other. Everything below travels through the
shared library database.

```sh
bookclerk nodes list                              # liveness, capacity, budget, use, labels
bookclerk nodes set --memory-budget 50            # this node (or pass a node id)
bookclerk nodes set <node-id> --drain             # move scheduled replicas away
bookclerk nodes label --add nas --add zone=eu
bookclerk nodes owner <node-id> --user alice      # alice may now set its budgets and drain it
bookclerk plugins deploy example --replicas 2 --on nas --on '!slow'
bookclerk plugins deploy example --replicas all   # every node with room
bookclerk plugins deploy example --replicas 0     # keep the placement, stop every replica
bookclerk plugins deploy example --manual         # back to deploy/undeploy by hand
```

### Heartbeats and budgets

Every daemon writes a heartbeat row to `node_status` every 15 seconds. It
reports:

- **Capacity** the node detects: logical CPUs and memory, each capped by
  the daemon's cgroup (`cpu.max`, `memory.max`) when it runs in one.
- **Budget**: the share of that capacity the scheduler may commit to
  plugin guests, from the node's `node.runtime` document. The defaults
  are 80% of the CPU and 75% of the memory, so a desktop keeps room for
  its user. Set them with `bookclerk nodes set --cpu-budget/--memory-budget`,
  the nodes API, or **Settings → Nodes**. The node applies a change at its
  next heartbeat.
- **Reservations**: the sum of the jail ceilings (memory and CPU, after
  `[plugins.jail]` limits) of the guests running there. The reconciler
  records each guest's ceiling on its deployment observation when it
  spawns it.
- **Measured use**: CPU busy since the previous heartbeat, and memory
  available, for the whole machine (or the daemon's CPU-limited cgroup on
  Linux).
- **Availability**: how much of the recent past the node was up, as a
  decaying average over about a day. A desktop that is off every night
  ends up around 60%; an always-on VPS stays near 100%.

Every desktop platform reports memory, measured use and free disk:

| Platform | Memory (total / available) | CPU busy | Free disk |
| --- | --- | --- | --- |
| Linux | `/proc/meminfo` `MemTotal` / `MemAvailable`, capped by cgroup `memory.max` | `/proc/stat`, or cgroup `cpu.stat` under a CPU quota | `statvfs` |
| macOS | `hw.memsize` / free + inactive pages (`HOST_VM_INFO64`) | `HOST_CPU_LOAD_INFO` ticks | `statvfs` |
| Windows | `GlobalMemoryStatusEx` `ullTotalPhys` / `ullAvailPhys` | `GetSystemTimes` | `GetDiskFreeSpaceExW` |

> [!NOTE]
> Only Linux reads a container ceiling (cgroup v2). A daemon confined by a
> Windows Job Object or a macOS resource limit is measured against the
> whole machine.

A node is **live** while its last heartbeat is younger than three
intervals (45 s). A node where only the CLI ran has no heartbeat and is
never live.

**Draining** a node (`nodes set --drain`, or the Drain button) stops the
scheduler placing anything there and moves its scheduled replicas
elsewhere. Deployments made by hand keep running.

A node's **headroom** is its budget minus what is committed:

- For CPU, committed is the larger of the reservations and the measured
  use, so work outside Bookclerk on a shared desktop counts.
- For memory, headroom is the budget minus the reservations, and never
  more than the memory the machine has free.

### Candidates

A node qualifies for an instance when:

- it is live and not drained;
- it carries every label the constraints name, and none that a `!label`
  constraint names;
- for an installed plugin, the active version has an artifact for the
  node's target, or a target-independent `any` artifact (workerd
  packages).

Labels are `name` or `name=value` (lowercase letters, digits and `._/-`).
Every node also carries `node=<node_id>` and `target=<target>`
(`linux-x64-gnu`, …). So `--on node=<id>` pins an instance, and
`--on target=linux-arm64-gnu` restricts it to one platform.

### Planning

The scheduler estimates what one guest of the instance needs, its
**demand**. That is the largest ceiling any node recorded for it, else
the active version's grant (`memoryMib`, `cpuRatePercent`), else the grant
defaults (512 MiB, 0.8 cores).

It scores each qualifying node by how many more such guests fit its CPU
and memory headroom, weighted by availability. An always-on node counts in
full, and a node that is never on counts half. Then:

- Replicas stay where they run while their node qualifies. Nodes that
  stopped qualifying (dead, drained, label removed) get `unplaced`.
- New replicas go to the highest-scoring nodes, and only where the demand
  fits the memory headroom. Memory is the hard limit, because a guest over
  its memory ceiling is killed; CPU only ranks.
- A scale-down drops the replicas on the nodes with the least room first.
- When a replica's node is over its memory budget and another qualifying
  node has room, the replica moves there. At most one moves per instance
  per pass, so the cluster doesn't thrash.
- `--replicas all` runs the instance on every qualifying node where it
  fits.

Each pass commits the demand of every replica it adds against that node's
headroom, so one pass doesn't stack several instances on the same node.

When fewer nodes qualify or fit than requested, the scheduler places what
it can and records why on the placement, for example: "1 of 2 replica(s)
placed: 2 node(s) are live, schedulable, and match nas; 1 lack memory
budget for 512 MiB".

Each node's reconciler then converges on its own deployment rows, exactly
as it does for hand-written ones.

### Routing deliveries

Event and queue deliveries addressed to an instance go to the best live
node running it, without any node contacting another.

1. Every node that runs an instance ranks the live nodes whose guest of it
   is up (a `running` or `healthy` observation), by the same score
   placement uses. So all nodes agree on the order.
2. The best node claims a delivery as soon as it is due. The node ranked
   `r` claims it only once it has been due for `r × 10 s`.
3. Ranks are re-read every 10 seconds.

A best node that is saturated, stalled or gone therefore costs at most a
short delay before the next one takes over. The claim's compare-and-swap
still guarantees one runner.

Deliveries addressed to a plugin key or alias, and a node's own queue,
aren't routed.

> [!NOTE]
> Jobs (scan, acquire) aren't routed yet: whichever node claims a job runs
> it with its own sources and destinations. A synchronous HTTP call runs
> on the node that receives it.

### One scheduler

Every daemon runs the scheduler loop, but only the holder of the
`scheduler` row in `cluster_leases` acts.

- The holder renews the lease (60 s) on every pass. When it stops, the
  next daemon to try takes over, with a larger fencing token.
- A pass runs on a `placement.changed` or `install.changed` node-queue
  wakeup, and at least every 20 seconds.
- Deployment writes are compare-and-swap, so a holder that lost its lease
  mid-pass can at worst add one replica too many, which the next pass
  removes.

### Placed instances are the scheduler's

- `plugins deploy|undeploy` and the deployment API refuse a placed
  instance (409).
- The Settings Deployed switch is read-only for it.
- Enrollment records a new node `unplaced` for it, rather than `present`.

`plugins deploy --manual` (or deleting the placement) hands the
deployments back without changing what any node runs: `placed` becomes
`present`, and `unplaced` becomes `absent`. Uninstalling a plugin
(`plugins uninstall`, `DELETE /api/config/plugins/{key}`) drops its
placements first.

| Method | Path | Body / result |
| --- | --- | --- |
| `PUT` | `/api/config/plugin-instances/{id}/placement` | `{replicas, constraints, expected_revision}`. `replicas` is a count or `"all"`; `expected_revision` 0 (or omitted) creates; 409 on a stale revision |
| `DELETE` | `/api/config/plugin-instances/{id}/placement` | Back to hand-managed |
| `GET` | `/api/config/nodes` | Each node's `labels`, `implicit_labels`, `target`, `live`, `kind`, `owner`, `runtime` (budgets, drain) and `status` (latest heartbeat), plus the caller's `capabilities`. A node owner gets only its own nodes |
| `PUT` | `/api/config/nodes/{node_id}/runtime` | Any of `{label, cpu_budget_percent, memory_budget_percent, schedulable}`. A node owner may edit only its own nodes |
| `PUT` | `/api/config/nodes/{node_id}/labels` | `{labels}`: the node's complete operator label set (operator only) |
| `PUT` | `/api/config/nodes/{node_id}/owner` | `{user_id}` (or `null`): the user who administers the node (operator only) |
| `GET` | `/api/cluster/status` | Node counts, committed CPU and memory across live nodes, active jobs, delivery backlog, and the nodes the caller may see |

In the web app:

- `GET /api/config/plugin-instances` and the Settings plugin groups carry
  each instance's `placement` (requested and placed replicas,
  constraints, detail).
- **Settings → Plugins** shows "Placed n/m" on the row. Expanded, it has
  replica and label fields, and **Manage by hand**.
- **Settings → Nodes** shows each node's heartbeat, reservations and
  measured use against its budget, and availability. It has fields for
  the budgets and labels, and a Drain switch.

### Node-local instances

`plugins instances create <plugin> --node <node-id>` (or `node_id` on
`POST /api/config/plugin-instances`) creates an instance owned by that
node, typically a node's own `local` output.

- It deploys only on that node. A deployment to any other node, and any
  placement, are refused.
- It adds to the cluster's outputs on that node, rather than replacing
  them. Acquires a host node uploads reach every destination deployed on
  the owner node that received them, node-local ones included.

### Running on host nodes

A host node (a separate install that joined the cluster, see
[Libraries and branches](cluster-hosts.md)) runs an owner's plugin only
after **its own** operator approves it. The narrowing is the same as on
any host (see
[Consent before enable](../plugins/capabilities.md#consent-before-enable-and-every-spawn)).

To approve, use **Settings → Library** on the host, or:

```sh
bookclerk clusters approve <plugin_key> --request <DIGEST> [--deny-domain …] [--deny-binding …] [--deny-network]
```

`<DIGEST>` is the digest of the request being approved, from
`bookclerk clusters consents`.

- The owner's grant is the request. The host's approval may only remove
  from it.
- A later version that asks for more waits for approval again.
- A revoke cancels the plugin's jobs there and hands them back.
- The guest runs in the host's jail through `bookclerk-workerd`, as the
  host's own plugins do.

A platform plugin is approved automatically only when all of these hold:

- its key is a Bookclerk platform key;
- the host's own provenance check finds its install of that key
  platform-bundled;
- that bundled payload's digest equals the owner's artifact digest;
- the request stays inside the platform envelope (no network, only
  `config` and `work_fs`);
- the host's operator has neither revoked it nor approved it by hand. An
  operator's approval, which may narrow the request, is never replaced
  automatically.

## Host-local plugin namespace

What a plugin *is* (its installed versions, consent, instances, settings,
and where it is deployed) lives in the library database, shared by every
host in the cluster (see [Installing plugins](#installing-plugins)). What
a host *runs* is host-local: `$BOOKCLERK_FILES_DIR` holds the
materialized trees, guest state and the mutation lock for that host only.

```text
$FILES_DIR/
  node-identity.json             # this host's stable HostId
  plugins/.versions/<digest>/    # materialized installed version (read-only, shared by instances)
  plugins/.staging/              # partial extracts; never spawned from
  plugins/<PluginKey fs-id>/     # host-local tree: platform guests, dev trees, older installs
  plugin-packages/<name>/        # operator-placed package.json + archive.tar.gz
  plugin-state/<PluginKey fs-id>/data
  plugin-state/<PluginKey fs-id>/tmp
  plugin-state/<PluginKey fs-id>/session-<nonce>/  # host-owned; native-behind only
  plugin-grants.json             # consent for host-local trees (installed versions carry their own)
  install-ledger.json            # trust anchor for host-local installs only
  .plugin-mutation.lock          # OS advisory lock for cache and tree mutation
  .plugin-hold/                  # held trees/state during remove transactions
```

- The cache directory names a digest, not a plugin. Two instances of one
  version, or two plugins that ship identical bytes, share one tree.
- The `.versions` and `.staging` names start with a dot, so discovery and
  alias scanning skip them. An installed version is found through the
  database, never by walking `plugins/`.
- `.plugin-mutation.lock` is scoped to the files directory. Two hosts that
  share one PostgreSQL or D1 database each take their own lock and never
  block each other. Cluster coordination goes through database rows and
  compare-and-swap, not a shared-database advisory lock.

Host-local trees remain for three cases:

- platform guests that ship with the host (`sqlite`, `local`);
- development trees staged by `cargo stage-plugins` or `cargo dev`;
- installs committed before cluster installs existed.

Those use `plugin-grants.json` for consent and `install-ledger.json` for
identity. `receipt.json` inside such a tree is metadata only. Remove and
install identity is bound to the ledger plus the tree's place on disk
(`basename == PluginKey.fs_id()` for `pk-*` directories), so a tampered
receipt can't redirect `--purge-state` onto another PluginKey.

## Isolation modes

```toml
[plugins]
isolation = "required"  # required | best-effort | off
# jail_bin = "/usr/local/bin/bookclerk-jail"
```

These environment variables override the file:

| Variable | Sets |
| --- | --- |
| `BOOKCLERK_PLUGIN_ISOLATION` | The isolation mode |
| `BOOKCLERK_PLUGIN_JAIL` | The path to `bookclerk-jail` |
| `BOOKCLERK_PLUGIN_WORKERD` | The path to `bookclerk-workerd` |
| `BOOKCLERK_WORKERD_BIN` | The path to the pinned `workerd` |

The modes:

- **`required`** (default): a plugin that can't be jailed isn't loaded.
  The error names the reason and the plugin is skipped; the rest of the
  host runs.
- **`best-effort`**: start the guest unconfined when the platform or the
  installation can't support a jail, with a warning that says so per
  plugin.
- **`off`**: no jail. For development only.

> [!IMPORTANT]
> Isolation is only about the **OS jail**. The `bookclerk-workerd` front
> door is required in all three modes. `best-effort` and `off` still spawn
> every guest through `bookclerk-workerd`, and a missing launcher or
> `workerd` binary is a hard spawn error, not a fallback to direct native.

These are the same three modes as `[media].isolation`, for the same
reasons: the tiers differ in what they reach for, not in how a missing
jail is handled. Confirm what is in effect with
`bookclerk config get plugins.isolation`.

| Platform | Backend | File system | Syscalls | Network | Memory, CPU, PIDs |
| --- | --- | --- | --- | --- | --- |
| Linux | Landlock and seccomp-bpf | Allowlist, ABI-probed | Deny list | Per policy | cgroup v2 best-effort (`memory.max`, `cpu.max`, `pids.max`) when the spec sets limits |
| macOS | Seatbelt (`sandbox_init`) | Deny-default SBPL profile | Not applicable | Per policy | Not supported (file system and network only; no fake enforcement) |
| Windows | AppContainer | ACL allowlist at spawn | Not applicable | Capability SIDs | Job Object (spec fields override label heuristics) |

## Installing the launcher

The host looks for `bookclerk-jail` in this order:

1. `plugins.jail_bin` in `config.toml`
2. `BOOKCLERK_PLUGIN_JAIL`
3. `bookclerk-jail` beside the running executable

Each candidate must exist, so a stale configured path fails loudly
instead of quietly running an unconfined guest.

Build it and ship it with the hosts, with `bookclerk-workerd` when
shipping workerd guests:

```sh
cargo build --release -p bookclerk-cli -p bookclerkd \
    -p bookclerk-media-worker -p bookclerk-jail -p bookclerk-workerd
```

The Docker image ships the helpers in the read-only bundle under
`/opt/bookclerk`, next to `bookclerk` and `bookclerkd` (see
[Running Bookclerk → Docker](operations.md#docker)).

## Troubleshooting

### `refusing to run plugin <id> unconfined: bookclerk-jail not found in <dir>`

The launcher isn't installed beside the host binary. Install it, point
`plugins.jail_bin` at it, or accept unconfined guests with
`isolation = "best-effort"`.

### `refusing to run plugin <id> unconfined: this host cannot confine a process`

There is no backend: a kernel without Landlock, a container runtime that
blocks the Landlock syscalls, or a Windows host where an AppContainer
profile can't be created. The message names the backend that fell short.

### `bookclerk-jail: these allowlist paths do not exist: …`

A granted folder vanished between planning and launch. Both backends
reject a rule naming a missing path, and the launcher refuses rather than
quietly narrowing the jail.

### A plugin fails on a path it used to read

The path is outside the four grants. A guest that needs host data should
ask for it over RPC. The file system allowlist can't be negotiated from a
manifest.

### Everything fails right after spawn

Check the guest's stderr. The host inherits it, so it lands in the daemon
log directly under the `bookclerk-jail:` line that reports what was
applied.

### "Enable / spawn refused: run `bookclerk plugins approve` first"

The consent grant is missing, or the plugin widened `capabilities` since
the last grant. Approve again, then run `bookclerk plugins deploy <id>`.
Spawning also fails closed until the grant covers the current manifest.

## Next steps

- [Capabilities and consent](../plugins/capabilities.md): what a plugin
  may ask for.
- [The guest jail](../internals/guest-jail.md#the-guest-jail): how the
  jail is built.
- [Libraries and branches](cluster-hosts.md): running plugins on branches.
- [Stores reference](stores.md) and [Output and destinations](output.md):
  the first-party plugins' settings.
