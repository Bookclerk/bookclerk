# Clusters and host nodes

A Bookclerk **cluster** is one library database and the daemons that share
it. Those daemons are **owner nodes**: they hold the database credentials
and `master.key`. A **host node** is a separate Bookclerk install, with its
own database and its own operator, that joins an owner cluster to do work
for it: it runs the owner's scans and acquires on its own machine and sends
the results back. A host never holds the owner's database credentials or
DEK, and it runs an owner plugin only after its own operator approves it.

This page is the operator's guide to both sides. The design record is
[ADR: Control plane](adr/control-plane.md#host-node-enrollment-and-transport)
(H-4).

## On the owner: let hosts in

1. **Pick a transport.** Hosts reach the node API over TLS, except on
   loopback. Set `integrations.public_origin` to the address hosts will
   dial and choose a `[daemon.node_api]` mode (`acme`, `proxy`,
   `self_signed`, or `loopback`). The modes, their required settings, and
   what each refuses are in
   [Configuration → Host nodes](configuration.md#host-nodes). There is no
   plain-HTTP fallback: a LAN address over plain HTTP gets
   `403 transport_insecure`.
2. **Leave enrollment on** (`core.host_enrollment`, on by default; the
   operator or an Owner may turn it off with `bookclerk config set
   host_enrollment.enabled false`).
3. **Mint a link** for the user who will own the host:

   ```bash
   bookclerk nodes invite --user riley --ttl 24h   # prints the link once
   ```

   Any active user may also mint one for themselves with
   `POST /api/config/node-invites`. The link is
   `<public_origin>/enroll#bcn1.<link_id>.<secret>[.<spki_pin>]`. It is a
   single-use secret: the secret sits in the URL fragment (proxies and
   access logs never see it), the database keeps only its hash, and it is
   printed once. Hand it to the host's operator over a private channel.
   The Settings page has no invite screen on purpose, so the link never
   passes through the browser.

The host's node appears in `bookclerk nodes list` (`kind = host`), owned
by the link's user. **Trust follows that user's role**: a Member's or Host's
node receives only jobs whose store accounts belong to that user; an
Administrator's or Owner's node may receive any job. Revoke a host at any
time with `bookclerk nodes revoke <node_id>`: its socket closes with 4403
and it stops dialing.

## On the host: join

```bash
bookclerk clusters join            # prompts: enrollment link:
# or
bookclerk clusters join < link.txt # first line of stdin
```

`join` never takes the link on the command line, so it stays out of shell
history and process listings. It generates the node's Ed25519 signing key
and X25519 box key, redeems the link over the transport policy (a pinned
link trusts exactly the owner's self-signed key; otherwise the certificate
must chain to the public web roots), and only then seals both private keys
into this install's `encrypted_secrets` and records the link in
`cluster_links`. A link that is spent, expired, revoked, or simply not a
link is refused, and nothing is recorded.

**Restart `bookclerkd` after joining**: a running daemon dials links it
found at startup. From then on it connects to the owner, heartbeats every
15 s, and reconnects with backoff (1 s up to 60 s) whenever the connection
drops, resuming any job it still holds.

```bash
bookclerk clusters list            # cluster id, owner origin, node id, state
```

## Consent: what an owner may run here

The owner deploys plugin instances to the host like to any node, but the
host's daemon spawns none of them until its operator approves. Each
deployed plugin shows up as a **request**: the version the owner wants to
run and what that version asks for (network mode and domains, TCP targets,
address ranges, host bindings such as `config`, `secrets` or `kv`, and
memory/CPU/disk budgets).

```bash
bookclerk clusters consents [--cluster <id>]
bookclerk clusters approve <plugin_key> --version 1.2.0 \
    [--deny-domain cdn.store.example]... [--deny-binding secrets]... [--deny-network]
bookclerk clusters revoke <plugin_key>
```

The same is in the web UI under **Settings → Clusters** (operator only):
pending requests with their domains and bindings, a deny checkbox on each,
a deny-network switch, and Approve / Revoke. The HTTP routes behind it are
operator-only:

| Route | Body |
| --- | --- |
| `GET /api/config/cluster-consents` | → `{clusters, consents}` (links carry ids, origin, state and whether they are pinned; never keys) |
| `POST /api/config/cluster-consents/approve` | `{cluster_id, plugin_key, version, deny_domains?, deny_bindings?, deny_network?}` |
| `POST /api/config/cluster-consents/revoke` | `{cluster_id, plugin_key}` |

Rules:

- **Only what is asked for can be approved.** Denying a domain or binding
  the request does not ask for is refused (`409 approval_refused`, with the
  reason in `detail`).
- **The version you reviewed.** `--version` (the UI always sends it) makes
  the approval a compare-and-swap: if the owner has since moved to another
  version, the approval is refused rather than applied unseen.
- **Asking for more waits again.** A later version that asks for anything
  outside what was approved (a domain, TCP target, address range, binding,
  compatibility flag, entrypoint, a larger budget, or network at all) is
  `awaiting_host_reapproval` and does not run until approved. Asking for
  less needs nothing.
- **Platform plugins** are approved automatically (`approved_by =
  "platform"`) only when this host's own install of the same platform key
  is bundled with the same payload digest as the owner's artifact and the
  request stays inside the platform envelope.
- **Revoke stops work.** A revoke (CLI or UI) is seen within 5 seconds:
  the plugin's held jobs are cancelled and handed back to the owner unspent,
  and none of its jobs is claimed again until approved.

States: `consented`, `awaiting_host_consent`, `awaiting_host_reapproval`,
`invalid_grant`. The host sends the set of consented plugin keys with every
claim, so the owner never hands it a job it would refuse.

For an approved plugin the host fetches the owner's archive over the
signed artifact endpoint, checks its digest, and runs it jailed under the
approved grant through `bookclerk-workerd`, exactly as it runs its own
plugins ([Plugins → The guest jail](plugins.md#the-guest-jail)). There is no
in-process fallback.

## What a host runs

**Scans.** The guest's rows come back to the owner in fenced
`scan.results` batches (at most 200 rows; a row over 32 KiB is skipped).
The owner re-checks the job's fence and the node's trust on every batch,
refuses a whole batch if any row names an account outside the job (or, for
a Member's host, outside that user's accounts), and writes the rows through
its own store API. A host never writes library SQL.

**Acquires.** The host asks the owner for the job's titles
(`acquire.plan`). The answer, `acquire.targets`, lists the titles and every
output destination deployed on the owner node that answered, each with its
own naming options (templates, replacement characters, path limits). The
host acquires each title into a per-title outbox with one sub-directory per
owner destination, so each destination's file names come out exactly as a
local acquire on the owner would name them. It then **uploads** the title
(below). The host also keeps a best-effort copy of each title in its own
deployed destinations.

## Uploads

Files stream **through** the owner to its destinations; nothing is staged
on an owner node's disk.

1. `upload.grant` over the node socket, with a manifest: each distinct file
   content once (`size`, `sha256`, `content_type`) with a list of
   **copies**, one `{destination, key}` per owner destination. The owner
   answers `upload.granted` with an upload id and a bearer token **sealed
   to the host's box key** (only its hash is stored; it lives one hour).
2. `PUT /api/node/v1/uploads/{upload_id}/files/{n}` per file, with
   `Authorization: Bookclerk-Upload <token>`. The owner tees the body, chunk
   by chunk, into every destination under that destination's key, checking
   the size and SHA-256 as it streams and re-checking the job's fence about
   once a second. A destination sees the end of its stream only after the
   whole body matched; a mismatch, a lost fence, or a broken connection
   ends every destination's stream with an error, so each discards what it
   wrote.
3. `POST /api/node/v1/uploads/{upload_id}/commit`. One canonical batch,
   gated on the live fence, marks the title acquired, publishes
   `book_acquired`, and (for the job's last title) completes the job.

Refusals: `401 unauthorized` (unknown upload, wrong or expired token),
`421 misdirected` (another owner node granted it), `409 lost` (the fence is
gone), `422 refused` or `422 destinations_changed` (the owner node's
destinations are not the ones the manifest was written for; the upload is
aborted and the title fails on the host, to be retried). Limits: 512 files
per title, 8 GiB per file, 16 copies per file, 1 KiB keys. These routes sit
outside the cookie and operator-auth layers (the sealed token is the
credential) and run the transport policy first.

## Node-local instances

An instance can be **local to one node**: it deploys only on that node and
the scheduler never places it. The usual case is a node's own `local`
output (its own disk), which adds to the cluster's outputs on that node
rather than replacing them.

```bash
bookclerk plugins instances create local --name "nas disk" --node <node_id>
bookclerk plugins deploy <instance_id> --node <node_id>
```

`POST /api/config/plugin-instances` takes the same as `{plugin_key,
display_name, node_id}`. Deploying the instance to another node, or handing
it to the scheduler (`--replicas`), is refused. Uploads reach every
destination deployed on the owner node that granted them, node-local ones
included.

## Leaving

```bash
bookclerk clusters leave [--cluster <id>]
```

Leaving marks the link `left` and deletes its node keys, so this install
can never sign as that node again; a running daemon stops the link's
client within 5 seconds. The owner is not told: its operator should revoke
the node there (`bookclerk nodes revoke <node_id>`). Joining again needs a
new link.

## The node API

All of it is under `/api/node/v1/`, outside the browser session and
operator-auth layers, and every request first passes the transport policy.

| Route | Purpose |
| --- | --- |
| `POST enroll` | Redeem a link: `{link_id, secret, sign_pk, box_pk, software_version}` → `{cluster_id, node_id}` |
| `GET connect` | The node socket (WebSocket, subprotocol `bookclerk-node.v1`) |
| `GET artifacts/{sha256}` | An owner plugin archive, for a request signed `Authorization: Bookclerk-Node <node_id> <issued_at> <sig>` |
| `PUT uploads/{id}/files/{n}`, `POST uploads/{id}/commit` | Uploads (above) |

**Handshake.** `hello {node_id, incarnation, software_version, resume}` →
`challenge {nonce, cluster_id}` → `auth {sig}` (Ed25519 over the cluster,
node, incarnation, nonce and the owner's canonical origin, so a signature
relayed through another server fails) → `welcome`. An unknown node or a bad
signature closes with 4401, a revoked node with 4403, a newer connection
for the same node supersedes the old one with 4409, and a binary or
malformed frame closes with 4400.

**Frames** are JSON text, `{"v":1, "id", "type", ...}`, at most 1 MiB; a
request is answered by a frame with the same `id`. Both sides ping every
20 s and close after 60 s without a frame.

| Host → owner | Owner answers |
| --- | --- |
| `heartbeat` | `ok` (capacity and use land in `node_status`) |
| `claim {filter: {kinds, plugins}}` | `work` (job, lease, fence, sealed secrets) or `none {retry_after_secs}` |
| `renew` | `renewed`, or `lost` when the fence is gone |
| `complete`, `fail`, `release` | `ok`, or `lost` |
| `deployments.list` | `deployments` (what targets this node, with versions and the requested grant) |
| `instance.config` | `CONFIG`, and `SECRETS` sealed to the node only for a node trusted with every job |
| `observation` | `ok` (`materialized` or `error` for its own deployments) |
| `secret.update` | `ok` (a refreshed store token, sealed to the owner, only for a ref handed with a live job) |
| `scan.results` | `scan.accepted {batch, upserted}` |
| `acquire.plan` | `acquire.targets {targets, destinations}` |
| `upload.grant` | `upload.granted {upload_id, sealed_token, expires_in_secs}` |

The owner also sends `wake` frames (deployment, install, placement,
configuration and revocation changes), which the host acks. Secrets are
never frame text: job secrets, instance secrets and upload tokens are sealed
to the node's box key and bound to the node and the job or instance they
belong to; a write-back is sealed to the owner's box key.

## The live socket (`/api/live`)

The web UI holds one WebSocket to `GET /api/live` (subprotocol
`bookclerk-live.v1`) and re-reads a panel's ordinary routes when the daemon
says its topic changed, instead of polling on timers. The socket carries
topic names only (`library`, `jobs`, `deliveries`, `nodes`, `config`,
`consents`, `users`), never rows, ids or secrets:

```json
{"v":1,"type":"hello","topics":["jobs","nodes"]}
{"v":1,"type":"changed","topics":["jobs"]}
```

- **Origin** must match exactly: the configured `integrations.public_origin`
  or, without one, `http://<Host>` for a loopback `Host` with no
  forwarding headers. Anything else (or no `Origin`) is `403
  origin_mismatch`, also with `auth.enabled = false`, so another site's
  page cannot open it.
- **Session.** The operator, or a signed-in user. The operator hears every
  topic; an Owner or Administrator hears `library`, `jobs`, `deliveries`,
  `nodes` and `users`; anyone else hears `library` and `nodes`. The
  session is re-checked every 30 s, and a signed-out user's socket closes
  with 4401.
- Changes are found by reading revisions from the shared database every
  2 s (so a change made on any owner node is seen) and by in-process
  notices for immediate updates. The client sends nothing; a binary frame
  closes the socket.
- While the socket is down, the UI polls as it used to and reconnects with
  backoff (1 s up to 30 s, jittered). Behind a reverse proxy, forward the
  WebSocket upgrade (see [Operations → Reverse proxy](operations.md#reverse-proxy--tls)).

## Not built yet

- A link joined while the daemon runs is dialed only after a restart.
- Leaving does not notify the owner; revoke the node there.
- Uploaded objects carry content type, length and digest, but not the
  title metadata (ASIN, title, timestamps) a local acquire writes beside
  its files.
- Hosts receive jobs only: event deliveries, and the KV, queue and EVENTS
  bindings over the node socket, are not served to hosts. Instance
  `SECRETS` reach only a node trusted with every job.
- Pebble (ACME) does not run in CI.
