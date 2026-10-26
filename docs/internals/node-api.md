# Node API and the live socket

The wire protocols between the hosts of one library, and between the web app
and its daemon. Setting up branches and clerks is in
[Libraries and branches](../server/cluster-hosts.md); the decision
record is the [control-plane ADR](../adr/control-plane.md).

## The node API

All of it is under `/api/node/v1/`, outside the browser session and
operator-auth layers, and every request first passes the transport policy.

| Route | Purpose |
| --- | --- |
| `GET info` | The library: `{cluster_id, library_name, origin, transport, enrollment, spki_fingerprint?, software_version, ancestry}` |
| `POST enroll` | Redeem an invite: `{link_id, secret, sign_pk, box_pk, software_version}` → `{cluster_id, node_id}` |
| `POST enroll/oauth` | Redeem an approved sign-in: `{code, code_verifier, redirect_uri, sign_pk, box_pk, branch_name, software_version, connection?}` → `{cluster_id, node_id}`; for `connection: "clerk"` also `{connection, bootstrap}` (sealed to `box_pk`) |
| `GET connect` | The node socket (WebSocket, subprotocol `bookclerk-node.v1`) |
| `GET artifacts/{sha256}` | An owner plugin archive, for a request signed `Authorization: Bookclerk-Node <node_id> <issued_at> <sig>` |
| `PUT uploads/{id}/files/{n}`, `POST uploads/{id}/commit` | Uploads (see [Uploads](../server/cluster-hosts.md#uploads)) |

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
| `catalog.begin {share_books, include_branches}` | `catalog.session {fence, cursor, ancestry, destinations, wanted}` |
| `catalog.sync {fence, batch, cursor, done, titles}` | `catalog.accepted {batch, upserted, skipped, wanted}`, or `lost` |
| `catalog.upload {fence, store, product_id, files}` | `upload.granted` (then the upload routes above, job id `catalog`) |
| `node.leaving` | `accepted`, then close 4403 (the node is revoked) |
| `scan.request {account, plugin_instance_id}` | `scan.planned {created, duplicates, queue_full}`, or `error` `forbidden` / `invalid` |
| `job.status {job_ids}` (at most 64) | `job.statuses {jobs}`: only jobs this node may claim |

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
`consents`, `clusters`, `users`), never rows, ids or secrets:

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
  2 s (so a change made on any owner node, or by `bookclerk clusters
  join|leave`, is seen) and by in-process notices for immediate updates.
  The first read is taken before `hello`, so nothing written after the
  client hears `hello` is missed. The client sends nothing; a binary frame
  closes the socket.
- While the socket is down, the UI polls as it used to and reconnects with
  backoff (1 s up to 30 s, jittered). Behind a reverse proxy, forward the
  WebSocket upgrade (see [Operations → Reverse proxy](../server/operations.md#reverse-proxy--tls)).
