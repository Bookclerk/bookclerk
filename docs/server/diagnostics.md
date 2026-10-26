# Diagnostics & crash reports

Opt-in crash / error-burst reporting for Bookclerk operators. Product docs:
[documentation index](../README.md).

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

When sharing is on, these keys decide when a report is sent:

| Key | Default | Meaning |
| --- | --- | --- |
| `upload_on_crash` | `true` | Send the ring buffer from the panic hook |
| `upload_on_error_burst` | `true` | Send when ERROR events cross `error_burst_threshold` |
| `error_burst_threshold` | `10` | ERROR events inside the window that trigger a report |
| `error_burst_window_secs` | `60` | Sliding window for the error burst, in seconds |
| `upload_on_warn_burst` | `true` | Send when WARN events cross `warn_burst_threshold` |
| `warn_burst_threshold` | `20` | WARN events inside the window that trigger a report |
| `warn_burst_window_secs` | `60` | Sliding window for the warn burst, in seconds |
| `ring_buffer_capacity` | `200` | Most recent redacted events kept for a report (every level through TRACE) |

### Local verbosity vs ring buffer

The design behind this split is in
[ADR: Logging and tracing](../adr/observability.md).

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
`diagnostics/<version>/<report_id>.json`. Every diagnostics issue ends with
the IDs it was filed with, in an HTML comment GitHub does not render:

```
<!-- bookclerk-diagnostics: report ids
<report_id>
-->
```

The dashboard reads that block to link the reports to the issue, and
operators can still copy an ID from the raw body to pull the full object
from B2. People read the issue's summary comment instead (see
[Triage](#triage)): it shows what the issue tracks now, with dashboard links
(organization members only, by design), while the filed list stays as it was
filed even after a member detaches or moves a report. Issues filed before
the block existed list their IDs under a visible `### Report IDs` heading
(`` - `<id>` · [dashboard](<url>) ``); the dashboard reads that list only
when a body has no block.

## Triage

`diagnostics-ingest` sends the sanitized batch to the collector's `POST /triage`
(same key as `/report`). The Worker asks a small Workers AI model
(`TRIAGE_MODEL`, default `@cf/ibm-granite/granite-4.0-h-micro`, 131k-token
context) for an
issue plan as JSON. The model has no tools, so a crafted report can at most
shape that plan. The workflow then validates it before anything is filed: at
most five issues, fixed labels, only report IDs from the batch, bounded sizes,
links removed, and mentions and issue references defanged. Reports that do not
fit in the batch's 120,000-byte limit, reports a non-empty plan leaves out of
every issue, and every report of a run whose reply held no valid plan (or
that the budget or an outage kept from the model) go to one open **holding
issue**, `[diagnostics] untriaged reports`: the run appends their IDs to its
report block (`scripts/holding_issue.py` edits only that list;
when it holds 500 IDs, or was filed with the visible `### Report IDs` list
instead, a new one is opened) and comments once with the
reason, so every pulled report is tracked and nothing is buried in a new
issue per run; its summary comment, kept like every other issue's, is the
list people read. (An empty plan is the model deciding nothing needs an issue,
and files nothing.)

**Attaching to open issues.** When the dashboard database is bound, the
collector puts the newest open triaged issues (at most forty, number and
title) in front of the batch, and the model may answer with an `attach`
list instead of a new issue for a report that is the same failure as one
of them. The workflow validates that too: only issue numbers the collector
offered (its answer's `open_issues`), only report UUIDs from the batch,
each report under one issue, at most twenty issues, and a report the model
both attaches and files stays attached. Nothing is written to the issue:
the workflow sends the validated attachments to the collector's
`POST /issues/attach`, whose ledger records each report under its issue as
`triage` (an existing link or a member's exclusion for that pair stands),
and the next summary pass counts them on the issue's comment. Reports whose
issue was closed or unknown by then, and every attached report when the
collector cannot record them, go to the holding issue. Without the
dashboard the model sees no open issues and plans new issues only.

Ingest asks `/report` with `unfiled=1`. When the dashboard database is bound
the collector then does two things with its report ledger (see
[Member dashboard](#member-dashboard)): it passes over (and moves the cursor
past) reports some issue already tracks, so a re-pull from the start cannot
file them again; and a report whose error signature an open, triaged issue
already tracks, on the same `major.minor` version line, is linked to that
issue (`source = signature`) and not returned, so a known failure grows its
issue instead of reaching the model. A report whose signature only a
**closed** issue tracks is compared with that issue's fixed-in version (see
[Fixed-in version and regressions](#member-dashboard)): one that runs an older
version on the same `major.minor` line is linked to the closed issue the same
way and not returned, while one that runs the fixed-in version or a later one
is returned marked `regression_of` (the closed issue, its fixed-in version,
fix pull request and title; when several closed issues qualify, the one with
the newest fix). A closed issue on the same line with no fix recorded (closed
as not a bug or will not fix, or an earlier regression issue a member closed)
takes the report quietly instead, so a closed judgment is not reopened by a
new issue each time. Ingest then files the regressions without the model
(`scripts/regression_issue.py`): one `[diagnostics] Regression of #<n> in
<major.minor>` issue per closed issue and version line (at most ten per run;
the reports of further groups go to the holding issue), labeled
`diagnostics,bug,regression`,
whose body names the closed issue, the fix pull request and the fixed-in
version as plain references and lists the report IDs; the first line is a
marker naming the closed issue, which the dashboard reads so the issue shows
as a regression of it. The closed issue is not reopened. While a regression
issue is open, further reports on its line join it through the signature
link above; a report from the same line that arrives before the issue
exists is excluded from the model's batch and the holding issue. The pull log
prints every count (`linked`, `regressions`). A `force_full` run turns the
linking off and re-triages everything.

**Summary comment.** After the pull, every run (whether or not it pulled
anything) asks `GET /issues/summaries` for the issues whose figures changed
since their summary was last written and keeps **one comment per issue**,
marked `<!-- bookclerk-diagnostics: summary -->` on its first line, edited in
place (`scripts/update-summaries.sh` and the sans-IO renderer
`scripts/summary_comment.py`; editing a comment notifies nobody): how many
reports the issue tracks and from how many installations, how they were
linked (listed in the issue, matched by signature, added by triage, attached
by members), first and last receipt, the versions and platforms seen (the
top eight, the rest as "others") and the newest ten reports with dashboard
links. The figures come from the collector's ledger, each report counted
under the issue that tracks it, so a member's attach or detach on the
dashboard reaches GitHub on the next run. The collector fingerprints each
summary; the run echoes the fingerprint to `POST /issues/summary` once the
comment is written, and the issue is not handed out again until its figures
change (at most twenty issues per run). The comment edited is the first one
the workflow's own account wrote with the marker; a marker someone else posts
is ignored. The report block in a triage or regression issue's body is never
rewritten (only the ingest run appends to the holding issue's block, above).
Versions and platforms are untrusted report fields and are
validated or rewritten before they reach the comment; report IDs link only to
their own dashboard page.

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
edit; see [Cloudflare Worker deploys](../contributing/automation.md#cloudflare-worker-deploys).

`REPORT_API_KEY` exists only as a Worker secret and the
`DIAGNOSTICS_REPORT_API_KEY` Actions secret. Bookclerk builds carry only the
collector URL, so a release can submit reports but never read or triage them.

## Member dashboard

With the automation GitHub App's credentials set, the collector serves a
dashboard for members of the repository owner's organization: `/` signs in
with GitHub, and `/dash` (overview), `/reports` (filterable list),
`/reports/<report_id>` (one report with its event log), `/issues`
(diagnostics issues, their pull requests, the version each fix shipped in,
and reports that keep arriving from that version on) and `/issues/<number>`
(one issue's reports, with the corrections below) are member-only. Without the credentials the routes stay
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
organization, only that user can sign in. The page the member opened rides
along as a `next` path in the same cookie (dashboard pages only: `/dash`,
`/reports`, `/issues`, `/reports/<id>`, with a filter query), so sign-in
returns to it.

**Links from other sites.** The session cookie is SameSite=Strict, so a
browser leaves it off a navigation that starts on another site, such as a
report link in a GitHub issue, chat or email. A dashboard page opened that
way by a click, without a session cookie (`Sec-Fetch-Site: cross-site`,
`Sec-Fetch-Mode: navigate`, `Sec-Fetch-Dest: document`, `Sec-Fetch-User: ?1`),
answers with a tiny page that reloads the same address; that reload is
same-site, carries the cookie when there is one, and otherwise lands on the
sign-in page with the address kept as `next`. The reload is not
user-activated, so it can never repeat. Only clicked top-level GET page
loads get this: another site's scripts and frames still get the sign-in
page, owner actions stay POSTs with a same-origin `Origin` check, and
`/live` is never reloaded this way.

**What the Worker holds.** The App can write to the repository, so every
installation token the Worker mints is narrowed in the request to `metadata`,
`issues`, `pull_requests`, `members` and `contents` (for the release list
and compare reads below), all `read`, on this one repository; the App's
installation must grant each of them, or the token cannot be minted and
sign-in fails. The one write the Worker can ask GitHub for is an issue's
summary comment (the owner's **Update GitHub now** button, below): for that
call alone it mints a separate token narrowed to `issues: write` and nothing
else, lists the issue's comments and edits or posts the one comment with it,
and keeps no copy. It still cannot write pull requests, code or workflows,
and if the installation does not grant Issues: write the button reports that
and the scheduled run writes the comment instead.
The client secret and private key are Worker secrets only. Neither, nor any
user token, is logged, stored or sent to a browser, and
`scripts/tests/test_ci_contract.py` keeps them out of Bookclerk builds and
other workflows. The App needs the organization permission **Members: read**
(accepted on its installation; the repository permission **Issues: write**
the ingest Action already uses covers the owner's **Update GitHub now**
button) and the callback URL `https://diagnostics.<DOMAIN>/auth/callback`
(or the workers.dev URL) in its settings.

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
**Report ledger.** `report_issue` records which issue each report is linked
to and how (`source`): `filed` (listed in the issue body; the sync replaces
these rows from GitHub), `signature` (linked at ingest, above), `triage`
(the model attached it to an open issue, see [Triage](#triage)), `manual`
(a member attached it on the dashboard), and `excluded` (a member detached it from that issue; the sync,
signature matching and triage leave the pair alone from then on). Every count on the
pages reads this ledger: a report counts once, under the issue that tracks
it. A report two issues list is a conflict: the older issue keeps it, the
report page says so, and the list shows "+1" after the issue chip.

**Corrections.** Members fix the ledger from the dashboard; the pages are
plain forms that post to `/issues/link` from the same origin, every change
is written to the access log under the member's login, and open pages
refresh. The issue page lists the reports the issue body filed, the ones
linked later (by signature, triage or a member, each saying how and by
whom), the ones detached from it, and suggestions: indexed reports that
share a signature with the issue's reports and are linked to no issue.
**Detach** writes `excluded` for the pair (a filed report stays detached
when the sync next reads the body); **Attach** writes `manual`, replacing an
earlier exclusion since the member now asks for it, and excludes the report
from every other issue linking it, so the member's choice wins over the
oldest-issue rule above (the other issues' bodies still list the report;
their summary comments stop counting it, and **Attach again** there
reverses it); **Move** is an attach to the other issue, which takes the
report from every issue the same way, plus a detach from this one, in one
batch; **Merge into** attaches every report linked to the issue to the
chosen open issue, takes them all from every other issue, and detaches them
all from the first (the member closes the emptied issue on GitHub as a
duplicate; the Worker does not). **Dismiss** on a suggestion writes the
exclusion so ingest does not link it by signature later. The report page
offers **Detach** under each issue that lists it (how a conflict is
resolved) and an **Attach to issue** picker when none does. None of this
reaches GitHub directly: the issue page counts the ledger rows written since
the ingest Action last recorded the issue's summary comment
(`issues.summary_sent_at`), and the next run renders them into that
comment. Owners get **Update GitHub now** next to that count, which writes
that comment at once: the Worker renders the issue's summary with the same
renderer as the run (`dashboard/summary.ts`, a port of
`scripts/summary_comment.py` that the collector's tests hold to identical
output), edits the bot comment carrying the summary marker in place or posts
it, and records the fingerprint written so the run does not write it again.
A refusal is shown on the page (a locked or deleted issue is recorded as the
run would record it; anything else is left for the scheduled run). The two
writers take turns per issue: a summary handed to the run is not written by
the button until the run acknowledges it (or ten minutes pass), and an issue
a press holds is left out of the run's next hand-out, so neither writes over
the other's comment and then records the other's figures.

Every 30 minutes a cron lists issues labeled `diagnostics` changed since the
last run, reads the report IDs from each issue's hidden report block (or,
for an issue filed before it, the `### Report IDs` list), and asks GraphQL for the pull requests
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

**Fixed-in version and regressions.** For each closed issue with a merged
pull request, the same sync records the version the fix shipped in
(`issue_fix`, one row per change, the newest wins): it reads the repository's
whole release list, prereleases included (at most five pages of a hundred;
a longer list is left to `fixed-in` labels, since releases are listed by
creation time and one published late could hide past any shorter read), and
asks GitHub's compare API whether
the oldest release published after the merge contains the pull request's
merge commit (then the next two). The first that does is the fixed-in
version, with a leading `v` dropped, and a prerelease counts. Until one does
the fix is `unreleased`, and the next sync continues with the releases after
the last one compared, so a fix that only a later release carries is found
over a few runs and a new release is checked as soon as it exists. At most
five issues are resolved per sync, so a backlog clears over a few runs, and a
GitHub failure leaves the rows as they were. A `fixed-in:<version>` label on
the issue (`fixed-in:v0.4.2`, `MAJOR.MINOR.PATCH` with optional prerelease
and build) sets the version by hand and stops the automatic lookup for that
issue; the label's value wins over an earlier automatic row. A release row is
final: a later pull request that references the issue does not move it.

A report that shares a signature with an issue's tracked reports and runs the
fixed-in version or a later one (semver order; a build suffix or
distribution repack of the fixed version counts as it, a version that does
not parse never does) is a **regression**: the issues list counts them per
issue, the overview counts issues with any, and the report page marks such a
report "Regression" and names the fixed-in version. While a fix is
unreleased the count is instead the reports that arrived after the merge.
Reports from versions older than the fix are expected and not flagged.
Nothing reopens an issue automatically: a regression that reaches ingest
gets a new issue instead (see [Triage](#triage)), and the issues list marks
that issue "regression of #n" with a link to the closed one. The installation token the Worker
mints for these reads carries `contents: read` in addition to the issue,
pull request, member and metadata reads; it still cannot write.

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

## Ingest cursor

The collector keeps `diagnostics-ingest`'s place in the stored reports in its
D1 database: the run pulls with `GET /report?cursor=stored` and then stores
where the pull ended with `POST /report/cursor` (same key as `/report`), but
only once every report in the batch is filed, so a failed filing is retried by
the next run. A stored cursor only moves forward. A `force_full` run pulls from the start
(`since=0`) and leaves the stored cursor where it was.

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
| `diagnostics-ingest` | Daily `/report` pull, `/triage` on Workers AI, issues from the validated plan, regression issues, one summary comment per issue kept in place |
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
