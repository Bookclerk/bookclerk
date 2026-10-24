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

## GitHub Actions

| Workflow | Purpose |
|----------|---------|
| `diagnostics-collector-deploy` | Deploy Worker on `diagnostics.<DOMAIN>`, or on workers.dev when `DOMAIN` is unset (stores that URL in `DIAGNOSTICS_COLLECTOR_BASE_URL`); then check `/health` |
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

`CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `DIAGNOSTICS_REPORT_API_KEY`, `DIAGNOSTICS_B2_*`.

## Redaction & privacy

Exact-value redaction, upload abort, Worker heuristics, optional `CLIENT_IP_HASH_SALT`.
Uploads include `os`, `arch`, Linux/macOS distro string, and rustc channel/release.

Emails in remote uploads are **partially** masked (not fully `[REDACTED]`):

`address@sub.domain.tld` → `a*****s@***.d****n.tld`

Local part and registrable domain label keep first/last characters; intermediate
subdomain labels become `***`; the TLD is unchanged. Titles, paths, account ids,
and secrets remain fully redacted.
