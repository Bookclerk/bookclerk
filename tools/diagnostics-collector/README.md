# Diagnostics collector (Cloudflare Worker → Backblaze B2)

Deployed by **GitHub Actions** ([wrangler-action](https://github.com/cloudflare/wrangler-action)):
[`.github/workflows/diagnostics-collector-deploy.yml`](../../.github/workflows/diagnostics-collector-deploy.yml).

With the `DOMAIN` Actions variable set (`bookclerk.app` upstream) the Worker is
served on `https://diagnostics.<DOMAIN>`, and the ingest workflow and CI release
builds derive that URL from the same variable. Without `DOMAIN` (a fork using
its own Cloudflare account) the deploy turns on workers.dev, reads the URL from
the deploy output and stores it in Actions variable
**`DIAGNOSTICS_COLLECTOR_BASE_URL`** (organization first, repository as the
fallback).

| Method | Path | Who | Auth |
|--------|------|-----|------|
| `POST` | `/submit` | Bookclerk clients | none (validated + secret heuristics) |
| `GET` | `/report?since=<ms>&after=<name>` | GitHub Action | `Authorization: Bearer <REPORT_API_KEY>` |
| `POST` | `/triage` | GitHub Action | `Authorization: Bearer <REPORT_API_KEY>` |
| `GET` | `/health` | probes | none |
| `GET` | `/`, `/dash`, `/reports`, `/reports/<id>`, `/issues` | organization members | GitHub sign-in (`/auth/github`), session cookie |

The member dashboard is on only when the automation App's secrets are set;
see [Member dashboard](../../docs/diagnostics.md#member-dashboard). Its code
is under `dashboard/`, its D1 schema under `migrations/`, and its styles,
script and fonts under `public/static/`.

Each `/submit` is assigned a **`report_id` UUID**; the B2 object is
`diagnostics/<version>/<report_id>.json`. Triage issues list these IDs
for manual review.

### `/report` version filter

Before downloading objects, `/report` keeps only versions that match the latest
**stable** GitHub release for `GITHUB_REPOSITORY` (from `releases/latest`):

- equal to that baseline
- **newer** (semver greater — includes prereleases of a future version)
- **derivative** packaging of the baseline (`1.2.3+nix`, `1.2.3-1`, `1.2.3.fc40`)

If the repo has **no releases** yet (or `releases/latest` is 404), the filter is
disabled and all versions are returned. Ops can override with
`?baseline_version=` (empty = all). The ingest workflow always passes it,
resolved with its own GitHub token, because unauthenticated calls from
Cloudflare egress addresses are rate-limited (`403`).

Pagination uses `since` (upload timestamp ms) plus `after` (exclusive object
key within that timestamp) so a truncated batch cannot skip same-ms siblings.

## GitHub secrets

| Secret | Purpose |
|--------|---------|
| `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` | Deploy |
| `DIAGNOSTICS_REPORT_API_KEY` | Worker `REPORT_API_KEY` + ingest |
| `DIAGNOSTICS_B2_*` | B2 → Worker secrets |
| `AUTOMATION_APP_CLIENT_ID` / `AUTOMATION_APP_CLIENT_SECRET` / `AUTOMATION_APP_PRIVATE_KEY` | Optional: member dashboard sign-in and issue sync (Worker `GITHUB_APP_*`) |

### Workflow token permissions

Deploy's `GITHUB_TOKEN` needs only `contents: read`; after `wrangler deploy` it
checks `/health`. `GITHUB_TOKEN` cannot write repository variables, so in
workers.dev mode the URL is stored with an automation App token
(`AUTOMATION_APP_CLIENT_ID` / `AUTOMATION_APP_PRIVATE_KEY`, organization
and/or repository permission "Variables: write"); without those secrets the run prints the
`gh variable set` command to run by hand. Ingest needs only `contents: read` and `issues: write`.

### `/triage`

Takes the sanitized batch the ingest workflow builds and returns a Workers AI
issue plan (`plan_text`) plus the neurons it used. Bindings: `AI` and the
`TRIAGE_BUDGET` KV namespace (created on first deploy), which caps spend per UTC day
at `TRIAGE_DAILY_NEURONS`; see [`docs/diagnostics.md`](../../docs/diagnostics.md#triage).

### Member dashboard setup

1. In the automation App's settings: add the callback URL
   `https://diagnostics.<DOMAIN>/auth/callback` (or the workers.dev URL),
   generate a client secret, and add the organization permission
   **Members: read**; accept the new permission on the installation.
2. Add the client secret as Actions secret `AUTOMATION_APP_CLIENT_SECRET`
   (the client ID and private key secrets already exist).
3. Give the Cloudflare token **Account → D1 → Edit** and deploy.
4. Sign in as an organization owner and press **Index older reports** until
   it reports the reindex finished.

Run the tests with `npm test` (Node 22.13 or newer: the dashboard tests use
`node:sqlite` as a stand-in for D1).

## Bookclerk client

Bake the collector URL at build time (CI does this automatically):

```bash
BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL="https://diagnostics.bookclerk.app" \
  cargo build --release -p bookclerk-cli -p bookclerkd \
    -p bookclerk-media-worker -p bookclerk-jail
```

Then enable sharing without a config URL:

```toml
[diagnostics]
share_reports = true
```

Override anytime with `collector_url` or runtime `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL`.

## Ingest

[`.github/workflows/diagnostics-ingest.yml`](../../.github/workflows/diagnostics-ingest.yml) reads
`https://diagnostics.<DOMAIN>`, or `DIAGNOSTICS_COLLECTOR_BASE_URL` when
`DOMAIN` is unset.

See [`docs/diagnostics.md`](../../docs/diagnostics.md).
