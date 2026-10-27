# Libraries and branches

This page is the operator's guide to joining Bookclerk installs together:
letting branches in on a library, joining one from a branch, adding a
clerk, and what each side runs and shares.

## Libraries, branches and clerks

- A **library** is one library database and the daemons that share it.
- A **branch** is a separate Bookclerk install, with its own database and
  its own operator, that joins a library to do work for it. It runs the
  library's scans and acquires on its own machine and sends the results
  back. A branch never holds the library's database credentials or DEK,
  and it runs one of the library's plugins only after its own operator
  approves it.
- A **clerk** is the other way to join: an empty install that shares the
  library's database and keys and serves the same library. See
  [Clerks](#clerks).

The web UI says *library* and *branch*. The code, the API and the CLI keep
the older names:

| Web UI | Code, API and CLI |
| --- | --- |
| Library | Owner cluster (`clusters`) |
| The library's own daemons, clerks included | Owner nodes (`kind = owner`) |
| Branch | Host node (`nodes`, `kind = host`) |
| Branch invite | Enrollment link |
| "Let new branches join" | `core.host_enrollment` |

The design record is
[ADR: Control plane](../adr/control-plane.md#host-node-enrollment-and-transport)
(H-4).

## First start

A fresh install, with no Owner or Administrator yet and no library joined,
opens a welcome screen for its operator with two choices:

- **Start a new library**: the usual first run. Settings → User
  management sets up the first Owner, and you carry on from there.
- **Join a library**: the wizard [below](#on-the-branch-join-a-library).
  The install keeps its own operator, its own database and its own
  consent screen, and needs no local users first.

The welcome screen is operator-only, reached the way Settings is (loopback,
the tray, or the operator token). Choosing "Start a new library" closes it
in that browser. Later, **Settings → Library → Join a library** opens the
same wizard.

## On the library: let branches in

1. **Pick a transport.** Branches reach the node API over TLS, except on
   loopback. Set `integrations.public_origin` to the address branches will
   use (the same one people open the library at), and choose a
   `[daemon.node_api]` mode: `acme`, `proxy`, `self_signed` or `loopback`.
   The modes, their required settings and what each refuses are in
   [Configuration → Host nodes](configuration.md#host-nodes).
2. **Let new branches join.** **Settings → Branches** (operator or Owner)
   has the switch, on by default, and shows the address to join. In
   `self_signed` mode it also shows the certificate fingerprint that a
   joining operator checks.

> [!IMPORTANT]
> There is no plain-HTTP fallback. A LAN address over plain HTTP gets
> `403 transport_insecure`. In `acme` and `self_signed` mode the TLS
> listener serves the whole web app, not only the node API, because a
> joining branch's user signs in there.

To stop new branches joining from the command line:

```sh
bookclerk config set host_enrollment.enabled false
```

Turning it off stops new sign-ins and invites. Branches already joined
keep working.

### What a branch is trusted with

A branch's node appears among the nodes (`bookclerk nodes list`,
`kind = host`), owned by the user who signed in, or the invite's user.
**Trust follows that user's role**:

- A Member's or Host's branch receives only jobs whose store accounts
  belong to that user.
- An Administrator's or Owner's branch may receive any job.

Revoke a branch at any time from **Settings → Nodes** or with
`bookclerk nodes revoke <node_id>`. Its socket closes with 4403 and it
stops dialing.

### Branch invites

Joining with a sign-in needs no link. A branch invite is the other way in,
for a branch whose operator can't sign in to the library themselves.

To make one in the web app:

1. Open **Settings → User management**, then a user's actions, then
   **Add a branch**. The operator or an Owner may make one for anyone.
   Every signed-in user also has **Add a branch** in their own menu, for
   a branch of their own.
2. Copy the invite. The dialog shows it **once**, with a countdown to
   expiry (24 hours by default).
3. Hand it to the branch's operator over a private channel.

Below the invite are the user's outstanding invites, each with its expiry
and **Revoke**. Listings never carry a secret.

On the command line, this prints one invite once:

```sh
bookclerk nodes invite --user riley --ttl 24h
```

An invite looks like
`<public_origin>/enroll#bcn1.<link_id>.<secret>[.<spki_pin>]`. It is a
single-use secret:

- The secret sits in the URL fragment, so proxies and access logs never
  see it.
- The database keeps only its hash.
- It expires, and it can be revoked.
- The dialog's answer is sent `Cache-Control: no-store`. The invite is
  never kept in stored SPA state or logged, and is dropped when the
  dialog closes.

## On the branch: Join a library

Open **Settings → Library → Join a library**, or the welcome screen.

1. **Address.** Enter the library's address: `https://library.example`,
   or `host:port`, which means `https://`. This daemon, not the browser,
   reads `GET <library>/api/node/v1/info` under the transport policy. That
   gives the library's name, its canonical origin, its transport mode, and
   whether it lets new branches join.
   - If you typed another spelling of the library's address, the wizard
     offers the one it answers to.
   - If the library isn't accepting new branches, the wizard says so and
     stops.
2. **Fingerprint** (self-signed libraries only). The wizard shows the
   fingerprint of the certificate the library presented. Check it matches
   the one on the library's **Settings → Branches** page and tick "It
   matches". The fingerprint is pinned with the link (trust on first use).
   Other modes need the certificate to chain to the public web roots.
3. **Sign in.** Name the branch (the library's users see this name; it
   defaults to this machine's name) and choose **Sign in to <library>**.
   The browser goes to the library, which shows its normal sign-in
   (password, SSO, passkey, MFA) and then asks: "Let **<branch>** join
   **<library>** as yours?" The page says what joining means: the library
   can send that branch work for your accounts, and the branch's operator
   approves each plugin. Choose **Allow** or **Deny**.
4. **Connected.** The library sends the browser back to this install's
   `/join/callback`. The daemon finishes the join and starts the branch's
   connection at once, with no restart. The page shows "Connected to
   <library>" with a link to the
   [plugin requests](#consent-what-the-library-may-run-here).

**I have an invite**, the wizard's second tab, takes a pasted branch
invite instead. The SPA sends it straight to this daemon in a POST body
and keeps nothing. A pinned invite trusts exactly the library's
self-signed key.

### How the sign-in works

The library is already an OAuth authorization server (`/oidc/authorize`).
Joining uses it with one built-in public client, `bookclerk-branch`:

- It is never stored with the registered clients, and that id can't be
  registered.
- Its redirect URI must be a loopback URL (RFC 8252) or an `https://` URL.
- Its only scope is `node:enroll`, and it needs an S256 code challenge.

The exchange runs in five steps:

1. **Start** (`POST /api/config/join/start`, operator only). The joining
   daemon makes the PKCE verifier, the `state`, and the node's Ed25519 and
   X25519 key pairs, and keeps them sealed in a **pending join** for 10
   minutes. It answers only the authorize URL. The redirect URI is this
   install's own `/join/callback`.
2. **Consent** (on the library). `/oidc/authorize` checks the request and
   sends it to the consent page `/join/approve`. A malformed request is
   refused there and never redirected. Allowing checks that the signed-in
   user is active, that new branches may join, and that the connection
   passes the transport policy. Then it issues a single-use code that
   lives 60 seconds.
3. **Callback** (`POST /api/config/join/callback`). The SPA relays `code`
   and `state` to its own daemon. The daemon takes the pending join for
   that `state` (once) and redeems the code with the verifier only it
   holds. The code alone is useless, so no enrollment secret passes
   through the SPA.
4. **Redeem** (`POST <library>/api/node/v1/enroll/oauth`). The library
   consumes the code and checks its client, redirect URI, scope and
   verifier. It checks again that new branches may join and the user is
   active. Then it runs the **same canonical redeem batch** as an invite:
   one `nodes` row (`kind = host`, owned by the signed-in user),
   `node_credentials` and a receipt.
5. **Done.** The daemon checks that the answer names the library it read.
   It seals both private keys into its `encrypted_secrets`, records the
   link in `cluster_links`, and starts the link's client.

> [!NOTE]
> A retry of the same exchange (a lost answer) replays the same node, and
> two concurrent redeems still make one node. Anything else, such as a
> used, expired or unknown code, or a wrong verifier or redirect URI, is
> `403 enrollment_refused`.

### On the command line

```sh
bookclerk clusters join --url https://library.example [--name "Den PC"] [--fingerprint <fingerprint>]
bookclerk clusters join             # prompts for a branch invite
bookclerk clusters join < invite.txt
bookclerk clusters list             # library id, origin, node id, state
```

`--url` is the same sign-in, for an operator with a browser on the same
machine. The command:

1. Prints the authorize URL, and opens a browser when it can (set
   `BOOKCLERK_NO_BROWSER=1` to stop that).
2. Listens on a loopback port for the library's answer, for 10 minutes.
3. Records the link.

A self-signed library's fingerprint is confirmed with `--fingerprint` or
by typing `yes`.

Without `--url`, `join` reads a branch invite from stdin or a prompt. It
never takes an invite on the command line, so the invite stays out of
shell history and process listings.

A running daemon picks up a link joined on the command line within 5
seconds. From then on the daemon connects to the library, heartbeats every
15 s, and reconnects with backoff (1 s up to 60 s) whenever the connection
drops, resuming any job it still holds.

## Clerks

A clerk is a second (or third) server for the **same** library. It is an
owner node that opens the library's database with the library's key,
serves the web UI, plans jobs, and can take over maintenance leases when
another owner node goes down. A branch keeps its own database; a clerk has
none of its own.

### Who can add one

Only an **Owner** of the library. When an Owner signs in from the Join
wizard, the consent page asks "How should it join?", with **Join as a
branch** (the default) and **Join as a clerk of this library**. Everyone
else sees only the branch page.

Clerk is shown but disabled, with the reason, when:

- the library's database is the default SQLite file (only PostgreSQL or
  Cloudflare D1 can be opened by more than one server);
- the joining server didn't offer it (it isn't empty, see below);
- the Owner has neither an authenticator app nor a passkey.

### On the joining server

The Join wizard shows **Offer to join as a clerk** only on an empty
install. That means no users, store accounts, books or joined libraries of
its own, and no `BOOKCLERK_DATABASE_*` or `BOOKCLERK_D1_*` environment
override of the database settings. (`BOOKCLERK_D1_API_TOKEN` isn't one: it
stays this server's own.)

Ticking it asks you to confirm that the server's own database settings and
keys are replaced. The daemon checks again when the sign-in starts and
when it comes back.

> [!NOTE]
> For a D1 library, set this server's own `BOOKCLERK_D1_API_TOKEN` (the
> token is never sent between servers). For any clerk, install the
> library's database plugin (`postgres` or `d1`) first.

### Approving

Choosing clerk needs a fresh second factor on the consent page: the
6-digit code from your authenticator app, or your passkey. The code the
library issues is bound to the choice, so a code approved for a branch can
never be redeemed as a clerk. Consent and redeem need TLS (or loopback
with both ends on one machine), as for a branch.

### What is handed over

In its redeem answer the library sends the `[database]` connection and the
data-encryption key, sealed to the joining daemon's own X25519 node key.
It never sends the passphrase-wrapped `master.key` file. The browser never
sees either, and neither is logged.

The joining daemon writes them as its own bootstrap:

- `config.toml` `[database]`. A PostgreSQL URL goes into
  `database-postgres.url`, mode `0600`, named by `url_file`, and never
  into the TOML.
- `node-identity.json`, with the node id the library recorded.
- `master.key`, wrapped with this server's own `BOOKCLERK_AUTH_PASSWORD`
  (or `[auth].password`) when set, and unwrapped otherwise.

Then `bookclerkd` restarts itself cleanly. It stops its listeners as on
SIGTERM and re-executes the same binary with the same arguments and
environment (on Unix the PID stays the same). It comes back as an owner
node of the library. Sign in there with your library account.

### If the first start fails

The originals of those files are kept, as `<name>.pre-clerk` with a
`clerk-join.json` record, until that first start opens the library's
database.

If it can't (for example the PostgreSQL address only works on the
library's own machine, or the D1 token has no access to the database), the
start tries a few more times. Then it puts this server's own settings,
node identity and key back and stops. To recover:

1. Start it again to run as before.
2. Remove the clerk under **Settings → Clerks** on the library before
   joining again.

If a join or its undoing is cut short (a crash, a power cut), the next
start finishes undoing it, or refuses with instructions naming each file.

### On the command line

```sh
bookclerk clusters join --url https://library.example --as clerk
```

`--as` is `branch` by default. With `--as clerk` the CLI writes the same
bootstrap files and asks you to restart `bookclerkd`.

### Listing and removing

**Settings → Clerks** on the library (operator or Owner) lists every owner
node, this server first, and who approved it (`GET /api/config/clerks`).
Another user sees only the ones they own.

A clerk that isn't running can be removed there
(`DELETE /api/config/nodes/{node_id}`). Stop Bookclerk on it first.

> [!WARNING]
> Removing a clerk doesn't take back the database credentials or key it
> held. Rotate them if it is no longer trusted.

Audit events:

- On the library: `clerk_authorize`, `clerk_authorize_refused`,
  `clerk_enroll_refused` and `clerk_enrolled`.
- On the joining server: `clerk_joined`.
- When one is removed: `owner_node_removed`.

## Consent: what the library may run here

The library deploys plugin instances to the branch as to any node, but the
branch's daemon spawns none of them until its operator approves. Each
deployed plugin shows up as a **request**, with:

- the version the owner wants to run;
- what that version asks for: network mode and domains, TCP targets,
  address ranges, host bindings such as `config`, `secrets` or `kv`, and
  memory, CPU and disk budgets;
- the archive URL and digest this host would fetch;
- a `request_digest` covering all of it.

```sh
bookclerk clusters consents [--cluster <id>]
bookclerk clusters approve <plugin_key> --request <request_digest> \
    [--deny-domain cdn.store.example]... [--deny-binding secrets]... [--deny-network]
bookclerk clusters revoke <plugin_key>
```

The same is in the web UI under **Settings → Library** (operator only; an
Owner sees only the [sharing settings](#branches-of-branches)). It lists
pending requests with their domains and bindings, a deny checkbox on each,
a deny-network switch, and Approve and Revoke.

The HTTP routes behind it are operator-only:

| Route | Body |
| --- | --- |
| `GET /api/config/cluster-consents` | → `{clusters, consents}` (links carry ids, origin, state and whether they are pinned; never keys) |
| `POST /api/config/cluster-consents/approve` | `{cluster_id, plugin_key, request_digest, deny_domains?, deny_bindings?, deny_network?}` |
| `POST /api/config/cluster-consents/revoke` | `{cluster_id, plugin_key}` |

### Consent rules

- **Only what is asked for can be approved.** Denying a domain or binding
  the request doesn't ask for is refused (`409 approval_refused`, with the
  reason in `detail`).
- **You approve the request you reviewed.** `--request` (the UI always
  sends it) makes the approval a compare-and-swap. If the owner has since
  changed the request (another version, grant or archive), the approval
  is refused rather than applied unseen.
- **Asking for more waits again.** A later version that asks for anything
  outside what was approved is `awaiting_host_reapproval` and doesn't run
  until approved. That covers a domain, TCP target, address range,
  binding, compatibility flag, entrypoint, a larger budget, or network at
  all. Asking for less needs nothing.
- **Platform plugins** are approved automatically
  (`approved_by = "platform"`) only when this host's own install of the
  same platform key is bundled with the same payload digest as the
  owner's artifact, and the request stays inside the platform envelope.
- **Revoke stops work.** A revoke (CLI or UI) is seen within 5 seconds.
  The plugin's held jobs are cancelled and handed back to the owner
  unspent, and none of its jobs is claimed again until approved.

The states are `consented`, `awaiting_host_consent`,
`awaiting_host_reapproval` and `invalid_grant`. The host sends the set of
consented plugin keys with every claim, so the owner never hands it a job
it would refuse.

For an approved plugin, the host fetches the owner's archive over the
signed artifact endpoint and checks its digest. It runs it jailed under
the approved grant through `bookclerk-workerd`, exactly as it runs its own
plugins (see [The guest jail](../internals/guest-jail.md#the-guest-jail)).
There is no in-process fallback.

## What a branch runs

### Scans

The guest's rows come back to the owner in fenced `scan.results` batches,
of at most 200 rows (a row over 32 KiB is skipped). On every batch the
owner:

1. Re-checks the job's fence and the node's trust.
2. Refuses the whole batch if any row names an account outside the job,
   or, for a Member's host, outside that user's accounts.
3. Writes the rows through its own store API.

A host never writes library SQL.

### Acquires

1. The host asks the owner for the job's titles (`acquire.plan`).
2. The answer, `acquire.targets`, lists the titles and every output
   destination deployed on the owner node that answered, each with its own
   naming options (templates, replacement characters, path limits).
3. The host acquires each title into a per-title outbox, with one
   sub-directory per owner destination. So each destination's file names
   come out exactly as a local acquire on the owner would name them.
4. The host **uploads** the title, as described in [Uploads](#uploads).

The host also keeps a best-effort copy of each title in its own deployed
destinations.

## Asking the owner for a scan

```sh
bookclerk clusters scan [--cluster <id>] [--account <id>] [--instance <id>] [--no-wait]
```

The host's operator asks the owner cluster to queue a scan, through this
install's running daemon and its link
(`POST /api/config/clusters/{cluster_id}/scan`, a `scan.request` frame).
It runs as the user who owns this node:

- An Owner or Administrator scanning every account gets one job per
  scan-enabled account, spread over every owner node and host trusted with
  each.
- Anyone else scans only their own accounts. A named account that isn't
  theirs is refused (403).
- A node trusted with nothing (no owner, a disabled owner, or revoked) is
  refused.

The command then polls the jobs (`job.status`), which shows only jobs this
node would be trusted to claim.

## Uploads

Files stream **through** the owner to its destinations. Nothing is staged
on an owner node's disk.

1. **Grant.** The host sends `upload.grant` over the node socket, with a
   manifest. The manifest lists each distinct file content once (`size`,
   `sha256`, `content_type`), with a list of **copies**, one
   `{destination, key}` per owner destination. The owner answers
   `upload.granted` with an upload id and a bearer token **sealed to the
   host's box key**. Only its hash is stored, and it lives one hour.
2. **Send.** The host sends
   `PUT /api/node/v1/uploads/{upload_id}/files/{n}` per file, with
   `Authorization: Bookclerk-Upload <token>`. The owner tees the body,
   chunk by chunk, into every destination under that destination's key.
   It checks the size and SHA-256 as it streams, and re-checks the job's
   fence about once a second.
3. **Commit.** The host sends `POST /api/node/v1/uploads/{upload_id}/commit`.
   One canonical batch, gated on the live fence, marks the title
   acquired, publishes `book_acquired`, and (for the job's last title)
   completes the job.

A destination sees the end of its stream only after the whole body
matched. A mismatch, a lost fence or a broken connection ends every
destination's stream with an error, so each discards what it wrote.

| Refusal | Meaning |
| --- | --- |
| `401 unauthorized` | Unknown upload, or a wrong or expired token |
| `421 misdirected` | Another owner node granted it |
| `409 lost` | The fence is gone |
| `422 refused`, `422 destinations_changed` | The owner node's destinations aren't the ones the manifest was written for. The upload is aborted and the title fails on the host, to be retried |

Limits: 512 files per title, 8 GiB per file, 16 copies per file and 1 KiB
keys.

These routes sit outside the cookie and operator-auth layers, because the
sealed token is the credential. They run the transport policy first.

## Node-local instances

An instance can be **local to one node**: it deploys only on that node and
the scheduler never places it. The usual case is a node's own `local`
output (its own disk), which adds to the cluster's outputs on that node
rather than replacing them.

```sh
bookclerk plugins instances create local --name "nas disk" --node <node_id>
bookclerk plugins deploy <instance_id> --node <node_id>
```

`POST /api/config/plugin-instances` takes the same as
`{plugin_key, display_name, node_id}`. Deploying the instance to another
node, or handing it to the scheduler (`--replicas`), is refused.

Uploads reach every destination deployed on the owner node that granted
them, node-local ones included.

## Branches of branches

A branch is a full library of its own, so its users can join their own
branches to it. The result is a tree: A ← B ← C, where B is A's branch and
C is B's.

- **One hop at a time.** A knows B as a branch owned by one of A's users.
  A has no record of C and never learns it exists. A's work runs on B
  itself and is never passed on to C. No store account, store token or
  user of B or C is ever sent to A.
- **Books flow up.** B shares the books in its library with A: those
  acquired on B, and (when "Include books from my own branches" is on)
  those C shared with B. Each title carries its store, product id,
  metadata and file list, nothing else. A records them as belonging to the
  A user who owns B, so C's books are simply B's books there. A title B
  got from A isn't sent back.
- **Files** stream to A's outputs through the same upload endpoint a
  branch's acquire uses (nothing staged on A), under the keys they have on
  B. A title whose files would land on a key another book in A already
  uses is recorded, but its files aren't sent. If A has no output
  deployed, it records the titles without files.
- **Sync** runs when the branch connects, every 5 minutes, and at once
  when a setting changes. It is incremental and resumes after a
  reconnect. A full pass (after a setting changes, and once a day) also
  withdraws titles B no longer has.

### Sharing settings on the branch

**Settings → Library** on B has two settings for each library joined,
both on by default:

- **Share this branch's books with <library>.** Off: nothing new is sent,
  and A marks what it already got from B. Files already delivered stay in
  A's outputs.
- **Include books from my own branches.** Off: B shares only books
  acquired on B itself, and C's titles are withdrawn from A on the next
  pass.

B's operator or an Owner of B may change them. An Owner sees only the
libraries and these two settings; joining, leaving and plugin approvals
stay the operator's. Anyone else is refused (403).

The routes are `GET /api/config/join/sharing` (→ `{clusters}`) and
`POST /api/config/join/sharing`
`{cluster_id, share_books, include_branches}`.

### Branch titles on the library

**Settings → Branches** on A lists each branch with its owner, what it
shares (read-only, because B's operator or an Owner of B sets it), and
counts of its titles.

Titles from a branch that stopped sharing or left are kept, marked, until
the owner removes the entries with **Remove** (or
`POST /api/config/branches/titles/remove {node_id?}`). The files stay.

### No loops

`GET /api/node/v1/info` reports the library's ancestry: its own cluster id
and every library above it.

- Joining refuses a library whose ancestry contains the joining install's
  own cluster id (`409 join_cycle`). So neither A ← B ← A nor
  A ← B ← C ← A can be made.
- A tree holds at most 64 libraries along one path. A join that would take
  the joining install's ancestry past that is refused
  (`409 ancestry_too_deep`).

A clerk shares its library's database, so it is the same point in the
tree.

## Leaving

Open **Settings → Library → Leave library**, or run
`bookclerk clusters leave [--cluster <id>]`.

1. The branch tells the library first, with a `node.leaving` frame. The
   library revokes the node's credential, drops its live status, and
   closes the socket with 4403, instead of waiting for missed heartbeats.
2. The link is marked `left` and its node keys are deleted, so this
   install can never sign as that node again.
3. The link's client stops.

> [!NOTE]
> If the library can't be reached within 10 seconds, the branch leaves
> anyway and says the library wasn't told. Revoke the node there
> (**Settings → Nodes** or `bookclerk nodes revoke <node_id>`).

Joining again needs a new sign-in or invite. Books the branch shared stay
in the library, marked as from a branch that left (see
[Branch titles on the library](#branch-titles-on-the-library)).

## Not built yet

- The sharing settings are set in **Settings → Library** (the operator or
  an Owner). There is no CLI command for them yet.
- The library's name is the host of its public origin. There is no
  separate name setting.
- Uploaded objects carry content type, length and digest, but not the
  title metadata (ASIN, title, timestamps) a local acquire writes beside
  its files.
- Hosts receive jobs only. Event deliveries, and the KV, queue and EVENTS
  bindings over the node socket, aren't served to hosts. Instance
  `SECRETS` reach only a node trusted with every job.
- Pebble (ACME) doesn't run in CI.

## Next steps

- [Node API and the live socket](../internals/node-api.md): the
  `/api/node/v1/` routes, the node socket's handshake and frames, and the
  web app's `/api/live` socket.
- [Configuration → Host nodes](configuration.md#host-nodes): the
  transport modes.
- [Managing plugins](plugins.md): deploy plugins to nodes.
- [Branches](../guide/branches.md) on the Use Bookclerk shelf: joining a
  library in plain language.
