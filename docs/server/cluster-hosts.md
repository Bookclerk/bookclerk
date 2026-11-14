# Libraries and branches

This page is the operator's guide to joining Bookclerk installs together:
letting branches in on a library, joining one from a branch, adding a
branch on the library's own database, and what each side runs and shares.

## Libraries and branches

- A **library** is one library database and the daemons that share it.
- A **branch** is a separate Bookclerk install, with its own database and
  its own operator, that joins a library to do work for it. It runs the
  library's scans and acquires on its own machine and sends the results
  back. A branch never holds the library's database credentials or DEK,
  and it runs one of the library's plugins only after its own operator
  approves it.
- A **shared branch** is the other way to join: an empty install that
  shares the library's database and keys and serves the same library. Its
  operator is the library's operator. See
  [Shared branches](#shared-branches).
- A **clerk** signs in to a library as its person with a **library
  card** and shows that person's books. It isn't a node: the library
  never sends it work, and it holds no node key. See
  [Library cards](sign-in.md#library-cards).

The web UI says *library* and *branch*. The code, the API and the CLI keep
the older names:

| Web UI | Code, API and CLI |
| --- | --- |
| Library | Owner cluster (`clusters`) |
| The library's own daemons, shared branches included | Owner nodes (`kind = owner`) |
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
- An Administrator's or Owner's branch may receive any job, but takes
  only that user's own work unless the branch's operator turns on
  **Download for other members** for the library (see
  [Work settings on the branch](#work-settings-on-the-branch)).

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

A clerk's library card uses the same authorization server with a
second built-in public client, `bookclerk-client` (scope
`library:member`). Its code is redeemed at `POST /api/client/v1/token`
for a session token, not at the node API, and no node row is made. See
[Library cards](sign-in.md#library-cards).

### Store connections that follow the person

On the consent page the user may tick **Bring my store connections to this
branch** (off by default). The page lists the user's connected store
accounts and which of them would travel.

- **What travels.** The user's own connected store accounts whose
  storefront declares `credentials = "shareable"` in every `plugin.toml`
  the library knows for it, discovered or installed (see
  [`credentials`](../plugins/manifest.md#credentials-storefronts)), and
  those of a single-keeper store, such as Audible, that no other branch
  keeps yet. The branch then keeps that account fresh for the library
  (see [One refresh keeper](#one-refresh-keeper)). At most 64 accounts go
  with one join.
- **How.** The choice is kept on the enrollment link. The redeem answer
  carries the accounts and their secrets as one bundle sealed to the
  branch's X25519 box key (the same sealed box as the bootstrap for a shared branch,
  with its own associated-data domain). A retried redeem answers it
  again, but only while the user is still active and the branch has not
  been revoked. The library records each account as sent to that node in
  `account_shares` and audits `branch_connections_sent` (without the
  secrets). A bundle that can't be made is logged and left out: the
  branch still joins.
- **On the branch.** The daemon opens and checks the bundle, then keeps
  each account for the Owner who signed in to run the join, or else the
  branch's single active Owner. Each account is one atomic batch: the
  account row, its secrets re-sealed with the branch's own key, the link
  to the person, and a `received` row in `account_shares`. An account
  the person already has there keeps its own credentials but still gets
  the `received` row, so disconnecting everywhere reaches that library
  too. An account already linked to someone else on the branch is left
  as it is, and one
  that came without a secret is not added. The join's answer counts what
  was added, already there, taken, or not kept.
- **The branch operator's rule.** Each operator decides per member
  whether they may connect store accounts there (**Settings → User
  Management**, `PATCH /api/users/{id}` with `may_connect_accounts`). The
  rule is on by default, and nobody can change their own. A member it is off for adds nothing from a join,
  and the connect routes answer `403`. A sign-in that finishes after the
  rule was turned off is not linked, and an account it newly stored (or
  found revoked) is revoked again and its secrets deleted. A join already adding accounts when
  the rule is turned off (or the member is disabled) stops: each account
  is added in its own batch that checks the rule again, and the ones not
  yet added count as not kept.

### Adding connections later, and disconnecting everywhere

A branch's person can add store connections to a library the branch
already joined, and take one back from every library at once. The join
binds each link to its person: the Owner who signed in to run the join,
or else the branch's single active Owner (a join that can name neither
binds nobody). Only an active Owner may call these routes, and only
through the links they joined: anyone else, or an add through another
Owner's link, gets `403 not_branch_person`, and a disconnect leaves other
Owners' libraries alone.
Each call to a library is bounded at 12 seconds, inside the routes'
30-second deadline; the branch runs one such request at a time, so an
offer never reaches a library after a disconnect reported the account
gone.

- **Add to a library.** On the branch, **Accounts** shows **Add to
  *library*** beside each connection
  (`POST /api/config/join/connections` with `{cluster_id, accounts}`, at
  most 64). The branch picks the person's own connected accounts, records
  each as sent to `link:<cluster id>` in
  `account_shares`, and sends them over the link's socket as
  `accounts.add`: one bundle sealed to the connection's owner box key from
  `welcome`, bound to the cluster and node ids under its own
  associated-data domain, so a joining bundle never opens as an offer.
- **On the library.** The library keeps the accounts for the user its node
  row names as the branch's owner (the user who let the branch join),
  under its own per-member rule, exactly as a branch keeps a joining
  bundle: one guarded atomic batch per account, a `received` row against
  `node:<node id>`, and an audit row `branch_connections_added`. Accounts
  of storefronts not installed there are not kept, nor single-keeper
  accounts another branch already keeps. The answer,
  `accounts.added`, counts what was added, already there, taken, or not
  kept, and whether the rule refused them all. The library handles
  `accounts.add`, `accounts.remove`, `accounts.update` and the
  connections a join sends one at a time, so two branches never both
  become an account's keeper. It refuses a branch's request
  (`superseded`) unless the connection it came on is still the branch's
  live one, so a request the branch gave up on can't land after a later
  one.
- **Disconnect everywhere.** **Revoke everywhere** on the branch
  (`POST /api/config/join/connections/disconnect` with
  `everywhere: true`) disconnects it on the branch first, then sends
  `accounts.remove` up every link the account was shared with, in either
  direction, all at once. A link the branch left or lost is named in
  `unreached`. Each library removes the person's links to the account and, in
  the same atomic batch, deletes its secrets and marks it revoked when no
  other member's link holds it. When it is the only holder, the library
  first marks the account revoked, so a scan still running can't write
  more, then removes its holdings (each with a `holding_removed` event)
  and its scan checkpoints, as any revocation does, and only then the
  links: a failed step is retried by disconnecting again. A library that can't be reached is named
  in the answer's `unreached`, and the account stays connected there.
  **Revoke here** (or `everywhere: false`) disconnects it on the branch
  only.

### One refresh keeper

A single-keeper store (Audible) breaks a sign-in that two places refresh
on their own, so each such account has one keeper per library. Which side
keeps it follows from `account_shares`: the library sees the branch it
first recorded the account against (`node:<id>`), and that branch sees a
share with `link:<cluster id>`. A library with no such share keeps the
account itself.

- **On the branch.** The branch's copy is the one that refreshes: Accounts
  sign-ins, refreshes its own scans hand back, and refreshes a job for
  that library hands back are all saved there. Once a minute, and each
  time the link connects, the branch looks for accounts whose secrets
  changed since what it last pushed, and sends each up the link as
  `accounts.update`: one account sealed to the connection's owner box key
  like an offer, with a revision above the last one it sent there. Any
  secret write counts as a new sign-in (the branch compares a fingerprint
  of the secrets, not their times), so neither a clock that went back nor
  two writes in one millisecond can hide one. A push the library applies
  is recorded so it isn't sent again; one it already had a revision for
  (`stale`, as after a lost answer) is sent again above it.
- **On the library.** The library applies a push only from the account's
  keeper, for the person who owns the branch, and only when its revision
  is newer than the last one applied (`account_credential_revisions`), in
  one guarded atomic batch, and audits `branch_sign_in_applied`. A push
  retried or overtaken by a newer one changes nothing. Once the keeper's
  first push is applied, the library doesn't save its own scans' refreshes
  of the account, and refuses a job's write-back of it (`kept_by_branch`),
  so neither overwrites the keeper's newer sign-in. Until then the library
  keeps refreshing it itself, so a branch that was sent the account but
  couldn't keep it (no Owner to keep it for, or its member rule refused it)
  never leaves it stale.
- **Elsewhere.** A kept account travels back only to its keeper, never to
  a second branch, and a second branch's offer of it is not kept.
- **A new keeper.** When the account is disconnected everywhere on the
  library (no member's link holds it), the library forgets its keeper, so
  the branch the person adds it from next keeps it. Adding it again
  from a branch makes that branch push its current sign-in again, even
  an unchanged one, so the library takes it as keeper anew. A push the library
  doesn't take from the branch yet (`not_shared`, as when it overtook the
  branch's offer) is sent again on the next pass.

Disconnecting on a library does not yet reach its branches.

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

## Shared branches

A shared branch is a second (or third) server for the
**same** library. It is an owner node that opens the library's database
with the library's key, serves the web UI, plans jobs, and can take over
maintenance leases when another owner node goes down. Any other branch
keeps its own database; a shared branch has none of its own.

### Who can add one

Only an **Owner** of the library. When an Owner signs in from the Join
wizard, the consent page asks "How should it join?", with **Join as a
branch with its own database** (the default) and **Join as a branch on
this library's database**. Everyone else sees only the branch page.

The second choice is shown but disabled, with the reason, when:

- the library's database is the default SQLite file (only PostgreSQL or
  Cloudflare D1 can be opened by more than one server);
- the joining server didn't offer it (it isn't empty, see below);
- the Owner has neither an authenticator app nor a passkey.

### On the joining server

The Join wizard shows **Offer to join as a shared branch** only on an empty
install. That means no users, store accounts, books or joined libraries of
its own, and no `BOOKCLERK_DATABASE_*` or `BOOKCLERK_D1_*` environment
override of the database settings. (`BOOKCLERK_D1_API_TOKEN` isn't one: it
stays this server's own.)

Ticking it asks you to confirm that the server's own database settings and
keys are replaced. The daemon checks again when the sign-in starts and
when it comes back.

> [!NOTE]
> For a D1 library, set this server's own `BOOKCLERK_D1_API_TOKEN` (the
> token is never sent between servers). For any branch on the library's
> database, install the library's database plugin (`postgres` or `d1`) first.

### Approving

Joining as a shared branch needs a fresh second factor on the consent page: the
6-digit code from your authenticator app, or your passkey. The code the
library issues is bound to the choice, so a code approved for a branch can
never be redeemed for the library's database. Consent and redeem need TLS (or loopback
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

The originals of those files are kept, as `<name>.pre-shared-branch` with a
`shared-branch-join.json` record, until that first start opens the library's
database.

If it can't (for example the PostgreSQL address only works on the
library's own machine, or the D1 token has no access to the database), the
start tries a few more times. Then it puts this server's own settings,
node identity and key back and stops. To recover:

1. Start it again to run as before.
2. Remove the branch under **Settings → Shared branches** on the library before
   joining again.

If a join or its undoing is cut short (a crash, a power cut), the next
start finishes undoing it, or refuses with instructions naming each file.

### On the command line

```sh
bookclerk clusters join --url https://library.example --as shared branch
```

`--as` is `branch` by default. With `--as shared-branch` the CLI writes the same
bootstrap files and asks you to restart `bookclerkd`.

### Listing and removing

**Settings → Shared branches** on the library (operator or Owner) lists every owner
node, this server first, and who approved it (`GET /api/config/shared-branches`).
Another user sees only the ones they own.

A shared branch that isn't running can be removed there
(`DELETE /api/config/nodes/{node_id}`). Stop Bookclerk on it first.

> [!WARNING]
> Removing a shared branch doesn't take back the database credentials or key it
> held. Rotate them if it is no longer trusted.

Audit events:

- On the library: `shared_branch_authorize`, `shared_branch_authorize_refused`,
  `shared_branch_enroll_refused` and `shared_branch_enrolled`.
- On the joining server: `shared_branch_joined`.
- When one is removed: `owner_node_removed`.

## Consent: what the library may run here

The library deploys plugin instances to the branch as to any node, but the
branch's daemon spawns none of them until its operator approves. Each
deployed plugin shows up as a **request**, with:

- the version the owner wants to run;
- what that version asks for: network mode and domains, TCP targets,
  address ranges, host bindings such as `config`, `secrets` or `kv`, the
  hosts that keep the plugin's own
  [User-Agent](../plugins/capabilities.md#user-agent) with its reason, and
  memory, CPU and disk budgets;
- the archive URL and digest this host would fetch;
- a `request_digest` covering all of it.

```sh
bookclerk clusters consents [--cluster <id>]
bookclerk clusters approve <plugin_key> --request <request_digest> \
    [--deny-domain cdn.store.example]... [--deny-binding secrets]... \
    [--deny-own-user-agent api.store.example]... [--deny-network]
bookclerk clusters revoke <plugin_key>
```

The same is in the web UI under **Settings → Library** (operator only; an
Owner sees only the [sharing settings](#branches-of-branches)). It lists
pending requests with their domains, bindings and own-agent hosts, a deny
checkbox on each,
a deny-network switch, and Approve and Revoke.

The HTTP routes behind it are operator-only:

| Route | Body |
| --- | --- |
| `GET /api/config/cluster-consents` | → `{clusters, consents}` (links carry ids, origin, state and whether they are pinned; never keys) |
| `POST /api/config/cluster-consents/approve` | `{cluster_id, plugin_key, request_digest, deny_domains?, deny_bindings?, deny_own_user_agent_hosts?, deny_network?}` |
| `POST /api/config/cluster-consents/revoke` | `{cluster_id, plugin_key}` |

### Consent rules

- **Only what is asked for can be approved.** Denying a domain, binding or
  own-agent host the request doesn't ask for is refused
  (`400 invalid_approval`, with the reason in `detail`). Unless network
  access is denied too, so is denying an own-agent host that an approved
  wildcard still covers. A host the owner stops asking for is dropped from
  the approval.
- **You approve the request you reviewed.** `--request` (the UI always
  sends it) makes the approval a compare-and-swap. If the owner has since
  changed the request (another version, grant or archive), the approval
  is refused with `409 approval_refused` rather than applied unseen.
- **Asking for more waits again.** A later version that asks for anything
  outside what was approved is `awaiting_host_reapproval` and doesn't run
  until approved. That covers a domain, TCP target, address range,
  binding, compatibility flag, entrypoint, a larger budget, or network at
  all. Asking for less needs nothing. `clusters consents`, the web UI
  and the API (`pending` on each consent) list what changed since the
  approval, in the same words the
  [local consent flow](../plugins/capabilities.md#upgrades) uses.
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

### Plugin state on a branch

A branch keeps each joined library's plugin state apart from every other
library's and from its own. A guest that runs for a library gets
`plugin-state/link-<32 hex>/<PluginKey fs-id>/data` and `…/tmp`. The leaf
is the first 128 bits of the SHA-256 of that library's cluster id. The
branch's own guests keep `plugin-state/<PluginKey fs-id>/`. So when two
libraries deploy the same plugin, their guests never see each other's
caches, cookies or logins. The plugin code cache under
`plugins/.versions/` stays shared, because it is read-only and addressed
by digest.

The design record is
[ADR: Library catalog network](../adr/library-catalog-network.md).

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
   naming options (templates, replacement characters, path limits). Each
   title carries its library id on the owner, so the `<library id>` naming
   tag writes the owner's id rather than the host's scratch one.
3. The host acquires each title into a per-title outbox, with one
   sub-directory per owner destination. So each destination's file names
   come out exactly as a local acquire on the owner would name them.
4. The host **uploads** the title, as described in [Uploads](#uploads).

The host also keeps a copy of each of its owner's own titles in its own
deployed destinations. A title acquired for another member is copied
there only when **Keep copies of other members' books** is on (see
[Work settings on the branch](#work-settings-on-the-branch)). The owner
says which titles are whose with each job.

Once the owner has committed one of the owner's own titles, the host also
adds it to its own library: the title, acquired, with its copy's files,
held by the store account that bought it. This happens only when that
store account is connected on the host to its Owner (for example brought
along when joining). Another member's title is never added, even when its
copy is kept. A title added this way is marked as coming from that
library, so it is never shared back to it. A title acquired for two
libraries is marked for both and shared back to neither.

### Sharing this computer between libraries

A branch of several libraries splits its work between them, so no one
library can take the whole machine.

1. **The machine's job slots** come from the branch's own heartbeat: its
   CPU budget over 0.8 of a core and its memory budget over 512 MiB (what
   one guest is assumed to need), whichever is smaller, from 1 to 16. A
   drained branch (`bookclerk nodes set --drain`) has none, so its
   libraries stop handing it work too. Until the branch records its first
   heartbeat after starting (within seconds) it also has none.
2. **The branch's own work comes first.** Each job its own workers run
   takes a slot.
3. **The rest is split equally** between the libraries it is a branch of,
   one slot at a time in cluster id order. A library capped below its
   equal share leaves its unused slots to the others.

Each library sees only its share:

- The branch claims no more of the library's jobs than its share. A share
  that shrinks cancels nothing: jobs already running finish, and claims
  wait until enough of them have.
- The branch's heartbeat to that library reports its share of the
  machine's CPU and memory, and of that share only what the machine has
  free now, so the library's scheduler never counts on the whole machine
  or on headroom other work already took. The library then applies its
  own budget for the branch (`bookclerk nodes set` there) as it does for any
  node; the branch's budget decides how many slots there are. Free disk is
  reported whole.

For example, a branch of two libraries with four slots runs up to two
jobs for each. While it runs one job of its own, the first library (by
cluster id) keeps two slots and the second gets one.

**Settings → Library → Work for <library> on this computer** caps one
library: **Equal share** (the default), **None** (no work for that
library), or **Up to N jobs**. B's operator or an Owner of B may change
it. The route is `POST /api/config/join/capacity`
`{cluster_id, max_jobs}` (required; `null` for an equal share, 0 to 16), and the
listing routes report each running library's `job_share` now.

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
- **Books flow up.** B shares its owner's books with A: those acquired on
  B on a store account connected there to B's Owner, and (when "Include
  books from my own branches" is on) those C shared with B. B's other
  members' books stay on B, unless A requests one or A's operator turns on
  **Acquire all of this branch's members' books** (see below). Each title
  carries its store, product id, marketplace, metadata (with its ASIN,
  ISBN and kind) and file list, nothing else. A matches each one to the edition it already knows, as a
  store scan would, and records B as holding it, so C's books are simply
  B's books there, belonging to the A user who owns B. A title B got from
  A, or acquired for A and added to its own library, isn't sent back.
- **Files** stream to A's outputs through the same upload endpoint a
  branch's acquire uses (nothing staged on A), under the keys they have on
  B. A title whose files would land on a key another book in A already
  uses is recorded, but its files aren't sent. Neither are the files of an
  edition A already has (acquired there, or delivered by another branch),
  so each edition arrives once. If A has no output deployed, it records
  the titles without files.
- **Sync** runs when the branch connects, every 5 minutes, and at once
  when a setting changes. It is incremental and resumes after a
  reconnect. A full pass (after a setting changes, and once a day) also
  withdraws titles B no longer has.

### Sharing settings on the branch

**Settings → Library** on B has these settings for each library joined,
all on by default:

- **Share this branch's books with <library>.** Off: nothing new is sent,
  and A no longer counts B as holding what it got from B. Files already
  delivered stay in A's outputs. B also lists nothing in answer to A's
  catalog questions.
- **Include books from my own branches.** Off: B shares only books
  acquired on B itself, and C's titles are withdrawn from A on the next
  pass.
- **List my members' books to <library>.** Off: B answers A's catalog
  questions with its owner's books only (see
  [Shared catalog](#shared-catalog)).
- **Use this computer to download.** Off: B is a window of that library.
  It takes none of A's work and shows the books A holds for B's person
  instead (see [Window mode](#window-mode)).

B's operator or an Owner of B may change them. An Owner sees only the
libraries, these settings and each library's job cap (see
[Sharing this computer between libraries](#sharing-this-computer-between-libraries));
joining, leaving and plugin approvals stay the operator's. Anyone else is
refused (403).

The routes are `GET /api/config/join/sharing` (→ `{clusters}`) and
`POST /api/config/join/sharing`
`{cluster_id, share_books, include_branches, list_members_books?, download?}`
(`list_members_books` and `download` are left as they are when absent),
which also takes the [work settings](#work-settings-on-the-branch).

### Work settings on the branch

**Settings → Library** on B also has two work settings for each library
joined, both off by default:

- **Download for other members of <library>.** Off: B asks A only for
  work on the store accounts of the A user who owns B, its owner's own
  books (including jobs pinned to one plugin instance). On: B also takes
  work for A's other members, where A trusts B with it (an
  Administrator's or Owner's branch). A still hands B only
  work it is trusted with, so this setting never widens trust.
- **Keep copies of other members' books.** Off: a book B acquires for
  another member goes only to A's outputs. On: it is also written to B's
  own outputs, as B's owner's books always are. It only matters while the
  first setting is on.

They apply to the next job B asks for; a job already running finishes.
The same people may change them, through the same routes:
`POST /api/config/join/sharing` takes `work_for_members` and
`keep_member_copies` too, and a body that leaves one out keeps it as it is.

### Branch titles on the library

**Settings → Branches** on A lists each branch with its owner, what it
shares (read-only, because B's operator or an Owner of B sets it), counts
of its titles, and A's **Acquire all of this branch's members' books**
(see [Acquiring all of a branch's members'
books](#acquiring-all-of-a-branchs-members-books)).

When a branch stops sharing, withdraws a title or leaves, A no longer
counts it as holding those titles. Files it already delivered are kept.
When nothing else in A holds that book either, its files are marked as
from a branch that left, until the owner removes the entries with
**Remove** (or `POST /api/config/branches/titles/remove {node_id?}`). The
files themselves stay in A's outputs. A book a store account or another
branch in A still holds keeps its files as that book's. If the branch
shares the title again later, it is delivered again.

### Shared catalog

A library can ask its branches whether they can supply works its people
are looking at, without copying their catalogs. The answers say "available
through B", and nothing is fetched until someone asks for it.

1. **The library asks.** Each Discover candidate fetch ends with an ask,
   at most every 15 minutes per daemon: A stores one question per trusted
   branch and work (works of open wishes and Discover candidates) and
   wakes each branch. A question already waiting, or answered within a
   day, isn't asked again.
2. **The branch pulls the questions.** On the wake, when it connects, and
   every 15 minutes, B sends `catalog.questions` and gets up to 100
   questions. A question that goes unanswered for 10 minutes is sent
   again.
3. **The branch answers.** B looks each work up in its own library and
   sends `catalog.found` with the editions it holds: store, product id,
   the formats it has, and metadata. A work it can't supply is named as
   missing.
4. **The library records the answers.** Each edition becomes a listing in
   A's shared listing pool, from source `branch:<B's node id>`. A listing
   counts for two days after B last named it. A later answer that no
   longer names it expires it.
5. **Discover and search show them.** When a Discover feed or a catalog
   search page is read, each card or hit is matched to its work (the
   card's own work, or the work of a store listing A's pool holds for one
   of its store editions) and gets `available_through`: one entry per
   current listing from a trusted branch, with A's work id (what a
   request names), the branch's node id, its owner's display name, and the
   store and product id. The field is never
   stored with the ranked feed, so it is current on every read.

Questions name works by identifier only, because work ids differ between
libraries:

- A asks with the ASINs (and marketplace), ISBNs and OpenLibrary ids of its
  editions of the work. Titles and authors are never compared.
- B maps them to one of its works by a strong identifier (an ASIN or an
  OpenLibrary id), or, when none matches, by ISBNs that reach one work
  only. A question that reaches no work, or more than one, is answered as
  missing.
- A keeps an answered edition only when it lands on an edition A already
  has of the asked work. An answer can't add works or editions to A.
  An edition B names by ISBN only is dropped for now: the resolver places a
  weak identifier only on an expression with a known language and
  abridgement, which most store-scanned editions lack.

An answer names no store account, user, or other library. A asks only
branches it still trusts, and records answers only for questions it sent
that branch.

**What B lists.** B always lists its owner's books. It never lists books on a store account linked to no one, such as one a member disconnected. It lists its members'
books too while **List my members' books to <library>** is on (the
default, set per library in the sharing settings below). A member can keep
their own books out with **Settings → Account → List my books to joined
libraries** (`PUT /api/auth/profile/catalog-listing` `{"listed": false}`;
`GET` also returns `libraries`, the number of libraries B is currently a
branch of, and the app shows the setting only when it is above zero, and
`always_listed`, true for an Owner, whose books are always listed, so the
app shows that instead of the toggle). With **Share this branch's books with <library>**
off, B lists nothing to that library and answers every question as
missing.

> [!NOTE]
> Turning a listing setting off, or a member opting out, takes effect at
> B's next answer. A asks again a day after B last answered, and a listing
> stops counting two days after B last named it, so A can still show B as
> a source for up to two days.

### Requesting a book from a branch

When a work is available through a branch, any signed-in user of A can ask
for it. In the app, the title's details show **Available through Riley's
branch** with a **Request from Riley's branch** button. The API is
`POST /api/branch-requests` `{"work_id": "...", "node_id": "..."}`, where
`work_id` and `node_id` come from one of the work's `available_through`
entries; leave `node_id` out to ask the first branch that lists the work.

1. **A queues the request.** It becomes a `branch_request` job on A,
   pinned to B and naming the edition B listed (store, marketplace and
   product id) and the work's strong identifiers. A work no trusted branch
   currently lists is refused (404). Asking again while a request for the
   same work and branch waits returns that request (`already_queued`).
   A wakes B.
2. **B claims it.** On the wake, when it connects, and every 15 minutes,
   B sends `requests.claim` and gets up to 16 requests, each leased to B
   for 5 minutes. No other node, and no ordinary job claim, ever takes a
   request.
3. **B decides, automatically.** B accepts a request only for a book it
   still lists to A, under the same rules as its catalog answers (see
   **What B lists** above): its owner's books always, a member's while
   **List my members' books to <library>** is on and the member has not
   opted out, and nothing while **Share this branch's books with
   <library>** is off or B is a window of A (see
   [Window mode](#window-mode)). B's listing settings are where B's
   operator draws the line; there is no per-request approval.
4. **B fetches and sends the book.** An accepted book not yet downloaded
   is queued as an ordinary acquire in B's own library, on the store
   account on B that holds it. B records the book as requested by A, so
   its catalog sync sends it to A although it is a member's book. The book
   goes up with the sync's next pass after it is downloaded, and A holds
   it through B like B's other titles.
5. **A records the answer.** B answers each request with
   `requests.finish`: accepted (the job succeeds), not listed (the job
   fails as `not_listed` and isn't retried), or failed on B (the job fails
   as `branch_failed` and is retried up to the job's attempt limit). A
   request whose work has lost every strong identifier fails as
   `work_gone`.

A request names no store account, user, or other library, and A never
learns whose copy B sent. Requests made before B stopped listing a book
are answered "not listed".

> [!NOTE]
> A book B sends because of a request keeps going to A only while B
> still lists it: turning off **Share this branch's books** or **List my
> members' books**, or the member opting out, stops it at the next sync
> pass. Files A already received stay there.

### Acquiring all of a branch's members' books

By default A receives B's owner's books, plus the books A requests. To
receive every book B lists instead, A's operator (or an Owner) turns on
**Acquire all of this branch's members' books** for B in **Settings →
Branches**. The API is `PUT /api/config/branches/members-books`
`{"node_id": "...", "acquire": true}`.

- **B still decides what it lists.** B sends a member's books only while
  **Share this branch's books with <library>** and **List my members'
  books to <library>** are on and the member has not opted out, the same
  rules as its catalog answers.
- **Only books already downloaded on B are sent.** B doesn't download
  anything for this setting. A book a member downloads later goes up with
  the next sync pass.
- **A change restarts B's sync.** B picks up the new setting within one
  sync interval (5 minutes) and starts a full pass. Turned off, the
  members' books B stops sending are withdrawn when that pass ends. Files
  A already received stay there.
- Each change is written to A's security audit log
  (`branch_members_books_set`).

### Window mode

A window shows the books its person has in the libraries they joined,
without downloading anything. It is the same install as a branch with one
setting per library turned off: **Use this computer to download** in
**Settings → Library**. A person can start as a window and turn the
setting on later to keep copies.

With the setting off for library A, B:

- **Takes no work from A.** B claims no jobs from A and runs none of A's
  plugins. Each deployment A made to B is reported to A as `error` with
  the detail "this computer does not download for this library (window
  mode)", and B records, fetches and configures nothing for it. Turning
  the setting off hands back any job B holds for A at once. A's share of
  B's job slots goes to the libraries B downloads for, and A's job cap
  applies again when the setting is turned back on.
- **Lists nothing to A and takes none of its requests.** A window is a
  client of A, not one of its nodes. It answers A's catalog questions as
  "missing" and every interlibrary request from A as "not listed", so A
  shows no title as available through B.
- **Reads its person's books from A.** On every connection, every 30
  minutes, and at once when the setting changes, B sends `window.books`
  page by page. A answers each page (`window.page`) with the books on the
  store accounts linked to B's owner in A, at most 100 works per page,
  grouped by A's works with each work's identifiers. A answers only a
  branch it trusts, and the frame names no user, so B can read only its
  own person's books.
- **Keeps one copy of the list per library.** B stores each page as it
  arrives. Once the last page is in, it drops the books A no longer
  named. A read cut short keeps the list from the last complete read.
  Turning the setting back on clears that library's list.

B's **Library** page then shows **Your books in other libraries** to B's
operator and Owners: every library B is a window of, merged into one list
by work. Two books are the same work when their identifiers reach the
same work in B's own library, when they share a strong identifier (an
ASIN with its marketplace, or an OpenLibrary edition or work id), or when
one library groups them under one work. An ISBN alone doesn't join two
libraries' books, since one ISBN can sit on several editions. Titles are
never compared,
because work ids differ between libraries. Each entry shows which
libraries hold it, from which stores, and in which formats.

The route is `GET /api/config/join/window` (→ `{clusters, books}`), for
B's operator or an Owner of B.

> [!NOTE]
> Asking a library to send a book to a window isn't built yet. It arrives
> with window-mode requests, after interlibrary requests to branches.

### No loops

`GET /api/node/v1/info` reports the library's ancestry: its own cluster id
and every library above it.

- Joining refuses a library whose ancestry contains the joining install's
  own cluster id (`409 join_cycle`). So neither A ← B ← A nor
  A ← B ← C ← A can be made.
- A tree holds at most 64 libraries along one path. A join that would take
  the joining install's ancestry past that is refused
  (`409 ancestry_too_deep`).

A shared branch is the same point in the tree as its
library.

## Leaving

Open **Settings → Library → Leave library**, or run
`bookclerk clusters leave [--cluster <id>]`.

1. The branch tells the library first, with a `node.leaving` frame. The
   library revokes the node's credential, drops its live status, and
   closes the socket with 4403, instead of waiting for missed heartbeats.
2. The link is marked `left` and its node keys are deleted, so this
   install can never sign as that node again.
3. The link's client stops.
4. Once it has stopped, the branch deletes that library's plugin state
   (`plugin-state/link-<32 hex>/`). A Windows ACL journal still recorded
   there is revoked first; if that is refused, the state stays and the
   delete is tried again every few seconds.

A revoked branch deletes the library's plugin state the same way.

> [!NOTE]
> If the library can't be reached within 10 seconds, the branch leaves
> anyway and says the library wasn't told. Revoke the node there
> (**Settings → Nodes** or `bookclerk nodes revoke <node_id>`).

Joining again needs a new sign-in or invite. Files of books the branch
shared stay in the library, marked as from a branch that left (see
[Branch titles on the library](#branch-titles-on-the-library)).

## Not built yet

- The sharing settings and job caps are set in **Settings → Library**
  (the operator or an Owner). There is no CLI command for them yet.
- A branch answers only from its own library, not from other libraries it
  joined.
- A window can't ask a library to send it a book yet, and the library's
  scheduler can still place plugin replicas on a window (the window
  reports them as errors and never runs them).
- The library's name is the host of its public origin. There is no
  separate name setting.
- Uploaded objects carry content type, length and digest, but not the
  title metadata (ASIN, title, timestamps) a local acquire writes beside
  its files.
- Hosts receive jobs only. Event deliveries, and the KV, queue and EVENTS
  bindings over the node socket, aren't served to hosts. Instance
  `SECRETS` reach only a node trusted with every job.
- Pebble (ACME) doesn't run in CI.
- Store connections travel only with a sign-in join, not an invite.
- A library keeps a branch-kept account's sign-in only as fresh as the
  branch's last push: while the branch is offline, a token the branch
  refreshed is not yet there. Moving the keeper to another branch means
  disconnecting the account everywhere and adding it again.
- Adding connections later and disconnecting everywhere work only
  through links the join could bind to a person.

## Next steps

- [Node API and the live socket](../internals/node-api.md): the
  `/api/node/v1/` routes, the node socket's handshake and frames, and the
  web app's `/api/live` socket.
- [Configuration → Host nodes](configuration.md#host-nodes): the
  transport modes.
- [Managing plugins](plugins.md): deploy plugins to nodes.
- [Branches](../guide/branches.md) on the Use Bookclerk shelf: joining a
  library in plain language.
