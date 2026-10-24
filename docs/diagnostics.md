# Diagnostics & crash reports

Opt-in crash / error-burst reporting for Bookclerk operators. Product docs:
[documentation index](README.md).

## Operator config

The collector is served on `https://diagnostics.<DOMAIN>` when the `DOMAIN`
Actions variable is set (`bookclerk.app` upstream). A fork without a domain
gets its Cloudflare account's workers.dev URL instead, which the deploy stores
in Actions variable `DIAGNOSTICS_COLLECTOR_BASE_URL` (organization first, then
repository). Build Bookclerk with
that URL baked in:

```bash
BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL="https://diagnostics.bookclerk.app" cargo build --release -p bookclerk-cli
```

Release builds in CI pick the URL up automatically.

```toml
[diagnostics]
share_reports = true
```

Override with `collector_url` or runtime `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL`.

### Local verbosity vs ring buffer

- **stderr / OS facility** (journald, macOS `os_log`, Windows Event Log) honor
  `BOOKCLERK_LOG` → `RUST_LOG` → daemon default `bookclerk=info,warn`. The CLI
  default is quiet (`off`); use `-v` / `-vv` / `-vvv` or `BOOKCLERK_LOG`.
- Spawn stages and forwarded guest stderr are `bookclerk::spawn` tracing
  events, so they follow that filter and stay inside the JSON daemon sink.
  Raw `bookclerk-spawn:` lines are written only when `BOOKCLERK_SPAWN_DIAG` is
  `1`, `true`, or `stderr` (gateway CI sets this). The process-wide stage ring
  and the per-session stderr tail are byte-capped; plugin stderr is not kept
  in the stage ring after the session exits.
- The **diagnostics ring buffer always retains TRACE and above**, so crash /
  burst uploads include deep context even when the console is quiet.
- For local investigation: `BOOKCLERK_LOG=bookclerk=debug` (or `-v` / `-vv` on the CLI).

Uploads fire on: panic, ERROR burst, WARN burst, daemon/CLI job failure, or CLI
command failure (when sharing is enabled).

## Architecture

```text
bookclerk / bookclerkd  →  POST /submit  →  Worker (assigns report_id UUID)
                              ↓
                    B2: diagnostics/<version>/<report_id>.json
                              ↓
GitHub ingest → POST /triage (Workers AI, no tools) → validated plan → Issues

Organization members → GitHub sign-in → member dashboard (D1 index of the
B2 reports, issues and pull requests synced from GitHub every 30 minutes)
```

## Report IDs

Each `/submit` receives a UUID `report_id`. The B2 object is named
`diagnostics/<version>/<report_id>.json`. Triage issues list these IDs so
operators can pull the full object from B2 for manual review.

## Triage

`diagnostics-ingest` sends the sanitized batch to the collector's `POST /triage`
(same key as `/report`). The Worker asks a small Workers AI model
(`TRIAGE_MODEL`, default `@cf/ibm-granite/granite-4.0-h-micro`, 131k-token
context) for an
issue plan as JSON. The model has no tools, so a crafted report can at most
shape that plan. The workflow then validates it before anything is filed: at
most five issues, fixed labels, only report IDs from the batch, bounded sizes,
links removed, and mentions and issue references defanged. Reports that do not
fit in the batch's 120,000-byte limit are listed by ID in a separate fallback
issue, so every pulled report is tracked.

Spend is capped per UTC day by `TRIAGE_DAILY_NEURONS` (default 2,000, a fifth
of the account's 10,000 free Workers AI neurons a day; values above 9,000 are
clamped so a typo cannot reach paid usage). Each call reserves its
worst case (input bytes plus the 2,048-token output cap) in the
`TRIAGE_BUDGET` KV namespace (one key per UTC day, expiring after two days)
before the model runs and settles to the reported usage after; a call that
would exceed the cap gets `429`, and ingest files one issue listing the report
IDs instead. A call costs at most a few hundred neurons. KV has no atomic
increment and its reads can be up to a minute stale, so two calls close
together (concurrent, or back-to-back runs such as a scheduled run followed by
a manual one) can each read an old total and undercount. Each such lost update
is one call's cost, far inside the gap between the cap and the free
allocation. Wrangler creates the namespace on the first deploy (the binding
has no id), so that deploy's Cloudflare API token needs Workers KV Storage
edit; see [Cloudflare Worker deploys](ci.md#cloudflare-worker-deploys).

`REPORT_API_KEY` exists only as a Worker secret and the
`DIAGNOSTICS_REPORT_API_KEY` Actions secret. Bookclerk builds carry only the
collector URL, so a release can submit reports but never read or triage them.

## Member dashboard

With the automation GitHub App's credentials set, the collector serves a
dashboard for members of the repository owner's organization: `/` signs in
with GitHub, and `/dash` (overview), `/reports` (filterable list),
`/reports/<report_id>` (one report with its event log) and `/issues`
(diagnostics issues, their pull requests, and reports that keep arriving
after a fix merged) are member-only. Without the credentials the routes stay
off and `/` answers like `/health`. The deploy only uploads the credentials;
removing the Actions secrets later leaves the Worker's copies in place, so to
turn the dashboard off, delete the Worker secrets `GITHUB_APP_CLIENT_ID`,
`GITHUB_APP_CLIENT_SECRET` and `GITHUB_APP_PRIVATE_KEY` (Cloudflare dashboard
or `wrangler secret delete`).

**Sign-in.** The Worker sends the member to GitHub with a random state and a
PKCE challenge (kept in a 10-minute `__Host-bc_oauth` cookie), trades the
returned code for a user token, reads `GET /user`, and revokes that token at
once. It then checks `GET /orgs/<owner>/memberships/<login>` with its own
installation token; only `state: active` passes, and organization owners get
the owner tools. When the repository belongs to a user instead of an
organization, only that user can sign in.

**What the Worker holds.** The App can write to the repository, so every
installation token the Worker mints is narrowed in the request to `metadata`,
`issues`, `pull_requests` and `members`, all `read`, on this one repository.
The client secret and private key are Worker secrets only. Neither, nor any
user token, is logged, stored or sent to a browser, and
`scripts/tests/test_ci_contract.py` keeps them out of Bookclerk builds and
other workflows. The App needs the organization permission **Members: read**
(accepted on its installation) and the callback URL
`https://diagnostics.<DOMAIN>/auth/callback` (or the workers.dev URL) in its
settings.

**Sessions.** A random 32-byte `__Host-bc_session` cookie (HttpOnly, Secure,
SameSite=Strict) names a row in D1 that stores only its SHA-256. Sessions end
8 hours after sign-in, so a member removed from the organization loses access
within 8 hours; sign-out deletes the row, and owners can sign everyone out.
State-changing requests are POSTs that also need a same-origin `Origin`
header. Pages carry a strict Content-Security-Policy (only this origin's
styles, script, fonts and images; no inline script or style), `no-store`, and
`frame-ancestors 'none'`. Report and issue text is escaped everywhere.

**Data.** `DASHBOARD_DB` (D1, created on the first deploy; schema in
`tools/diagnostics-collector/migrations/`) holds one index row per report,
written by `/submit` after the B2 upload: version, platform, trigger, level
counts and a signature (a hash of the first error's target and its message
with numbers, IDs, paths, URLs and quoted text replaced; warnings when there
is no error). Raw messages stay in B2; the report page downloads that one
object with the Worker's B2 credentials, never `REPORT_API_KEY`. Because
`/submit` is open, indexing stops at `DASHBOARD_INDEX_DAILY_ROWS` reports a
UTC day (default 10,000; each report writes five D1 rows, so at most half of
D1's 100,000 free daily row writes, leaving the rest for sign-ins, the access
log and the sync); the report is still stored, and an owner's **Index older
reports** button (also used once after the first deploy) adds stored reports
the index is missing, up to 40 per press. The cron sync and the reindex stay
within Workers Free's 50 D1 queries and 50 subrequests per invocation: each
issue page, pull request batch or listing page is written in one or two
batches that also save the cursor, and work left over continues in the next
run or press.
Every 30 minutes a cron lists issues labeled `diagnostics` changed since the
last run, reads the report IDs from each issue's `### Report IDs` section (the
format triage and its fallback write), and asks GraphQL for the pull requests
that close or reference each issue (the newest 50 cross-references), up to
100 issues a run: changed issues first, then the rest in turn, least
recently checked first, so every issue is rechecked (an owner's **Sync issues
from GitHub now** button runs the same sync at once, and the ingest Action
calls `POST /issues/notify` with `REPORT_API_KEY` and `{"issue": <number>}`
after filing each issue, so the Worker re-reads that issue from GitHub and
links it immediately, reading again for a few seconds if GitHub does not
have the new issue yet; only the number is taken from the caller, and a
transferred issue is not followed); one that lost the `diagnostics` label is
dropped, and a failed GraphQL call keeps the links already stored. Each sign-in and report view is
written to an access log kept 90 days and shown to owners. The overview also
shows today's triage spend from `TRIAGE_BUDGET`.

**Live updates.** Open pages update on their own, without polling. Each page
opens a WebSocket to `/live` (same origin and a live session required), held
by one SQLite-backed Durable Object, `LiveHub` (binding `LIVE`, created by
the deploy's `[[migrations]]` entry), through the hibernation API so idle
pages cost nothing. When `/submit` indexes a report, `/issues/notify` links an
issue, or a sync or reindex stores something, the Worker tells the hub, and
the hub sends each page the bare text `changed` (syncs push only when the
stored issues, links or pull requests actually changed). The page then fetches its
own URL once with its session cookie and swaps in the elements marked
`data-live` (at most one fetch every 3 seconds, none while the tab is
hidden). A page that connects within 15 seconds of a change refreshes once,
covering a change made between its own reads and its connect. The push carries no data, and the hub never reads what pages send.
A live refresh of a report page skips the stored report, and skips the
access-log entry only when that member's view of the report is already
logged in the last 8 hours. Sockets close at the first push after the
session they were opened under expires, at once when an owner signs everyone
out, and a page whose refresh is sent to sign-in closes its own; the hub
keeps at most 200. Without the `LIVE` binding pages simply stay as
loaded. A Durable Object request is counted for each change, well inside
Workers Free's daily allowance because indexing is capped (above).

## Version filter on ingest

`GET /report` resolves the latest **stable** GitHub release (`releases/latest`)
and downloads only matching objects:

| Match | Example (baseline `1.2.3`) |
|-------|----------------------------|
| Equal | `1.2.3` |
| Newer | `1.2.4`, `2.0.0-rc.1` |
| Packaging derivative | `1.2.3+nix`, `1.2.3-1ubuntu2` |

With **no GitHub releases** yet, every version is eligible (current state of this
repo). Same-version prereleases like `1.2.3-rc.1` are excluded once `1.2.3` is
stable.

The daily ingest workflow resolves that release itself with its job token and
passes it as `?baseline_version=` (empty when there is no release), so the
Worker does not call GitHub on that path. Unauthenticated GitHub API calls from
Cloudflare's shared egress addresses are rate-limited and fail with `403`.

When a call to B2 or GitHub fails, the Worker answers `502` with
`{"error": "upstream failed"}` and logs the failing stage. Callers that send
the report key (ingest) also get a `detail` naming the stage, the HTTP status
and B2's short error code, for example `b2 list failed: HTTP 401:
unauthorized`; it never includes credentials or response bodies. `/submit`
callers get no detail.

## GitHub Actions

| Workflow | Purpose |
|----------|---------|
| `diagnostics-collector-deploy` | Deploy Worker on `diagnostics.<DOMAIN>`, or on workers.dev when `DOMAIN` is unset (stores that URL in `DIAGNOSTICS_COLLECTOR_BASE_URL`); apply dashboard D1 migrations; upload the dashboard's App secrets when set; then check `/health` |
| `diagnostics-ingest` | Daily `/report` pull, `/triage` on Workers AI, issues from the validated plan |
| `ci` | Release builds with `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` set to the collector URL |

### Workflow permissions (least privilege)

Permission keys follow GitHub’s parameterized names (hyphenated in workflow YAML).
See [repository permissions](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#repository-permissions).

| Workflow | Permissions |
|----------|-------------|
| Deploy | `contents: read`; in workers.dev mode an automation App token ("Variables: write") stores the URL |
| Ingest | `contents: read`, `issues: write` |

### Secrets

`CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `DIAGNOSTICS_REPORT_API_KEY`, `DIAGNOSTICS_B2_*`;
for the member dashboard, `AUTOMATION_APP_CLIENT_ID`, `AUTOMATION_APP_CLIENT_SECRET`
and `AUTOMATION_APP_PRIVATE_KEY` (GitHub's PKCS#1 key file as downloaded).

## Redaction & privacy

Exact-value redaction, upload abort, Worker heuristics, optional `CLIENT_IP_HASH_SALT`.
Uploads include `os`, `arch`, Linux/macOS distro string, and rustc channel/release.

Emails in remote uploads are **partially** masked (not fully `[REDACTED]`):

`address@sub.domain.tld` → `a*****s@***.d****n.tld`

Local part and registrable domain label keep first/last characters; intermediate
subdomain labels become `***`; the TLD is unchanged. Titles, paths, account ids,
and secrets remain fully redacted.
