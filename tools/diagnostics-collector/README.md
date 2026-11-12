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
| `GET` | `/report?since=<ms>&after=<name>[&unfiled=1]` | GitHub Action | `Authorization: Bearer <REPORT_API_KEY>` |
| `POST` | `/triage` | GitHub Action | `Authorization: Bearer <REPORT_API_KEY>` |
| `POST` | `/issues/attach` | GitHub Action | `Authorization: Bearer <REPORT_API_KEY>` |
| `GET` | `/health` | probes | none |
| `GET` | `/robots.txt` | crawlers | none (disallows everything, all content signals `no`) |
| `GET` | `/`, `/dash`, `/reports`, `/reports/<id>`, `/issues` | organization members | GitHub sign-in (`/auth/github`), session cookie |

The member dashboard is on only when the automation App's secrets are set;
see [Member dashboard](../../docs/server/diagnostics.md#member-dashboard). Its code
is under `dashboard/`, its D1 schema under `migrations/`, and its styles,
script and fonts under `public/static/`.

Each `/submit` is assigned a **`report_id` UUID**; the B2 object is
`diagnostics/<version>/<report_id>.json`. Triage issues list these IDs
for manual review.

## `/report` version filter

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
at `TRIAGE_DAILY_NEURONS`; see [`docs/server/diagnostics.md`](../../docs/server/diagnostics.md#triage).
With `DASHBOARD_DB` bound, the prompt also lists the newest open triaged
issues and the answer names them (`open_issues`), so the plan may `attach`
reports to them; the workflow sends the validated attachments to
`POST /issues/attach` (`{"links": [{"issue": n, "report_ids": [...]}]}`),
which records them in the report ledger and answers with the counts and the
IDs it could not link.

### Member dashboard setup

1. In the automation App's settings: add the callback URL
   `https://diagnostics.<DOMAIN>/auth/callback` (or the workers.dev URL),
   generate a client secret, and add the organization permission
   **Members: read**; accept the new permission on the installation. (The
   owner's **Update GitHub now** button uses the repository permission
   **Issues: write** the ingest Action already has.)
2. Add the client secret as Actions secret `AUTOMATION_APP_CLIENT_SECRET`
   (the client ID and private key secrets already exist).
3. Give the Cloudflare token **Account → D1 → Edit** and deploy.
4. Sign in as an organization owner and press **Index older reports** until
   it reports the reindex finished.

The Worker, dashboard and tests are strict TypeScript. Wrangler bundles the
`.ts` sources, so nothing is compiled. Check types with `npm run typecheck` (`tsconfig.json`
checks everything with the Workers and Node types; `tsconfig.worker.json`
checks the Worker's sources again with the Workers types only) and run the
tests with `npm test`. It runs Vitest (`vitest.config.ts`): the tests run
inside the Workers runtime through `@cloudflare/vitest-plugin`, on the
Worker's compatibility date and against real D1 databases with
`migrations/` applied, except the summary fuzz test, whose Python oracle
needs Node. `npm run coverage` adds a V8 coverage report. `public/static/assets/dashboard.*.js` is the browser script the dashboard
serves, not Worker code, and stays JavaScript.

### Cross-origin headers

Every answer is closed to other origins with explicit headers:
`withCrossOriginPolicy` in `cross-origin.ts` sets `CLOSED_HEADERS`
(`Cross-Origin-Resource-Policy`, `Cross-Origin-Opener-Policy` and
`Cross-Origin-Embedder-Policy` at their strictest, `X-Frame-Options: DENY`,
`nosniff`, `Origin-Agent-Cluster`, `X-Permitted-Cross-Domain-Policies: none`
and `X-XSS-Protection: 0`) and strips any `Access-Control-*` grant. No
browser on another origin needs the collector: the daemon posts `/submit`
from its own process, and the Actions call the key-protected routes
directly. `public/_headers` gives `/static/*` the same set, and
`cross-origin.test.ts` checks every route and the file.

### Caching

Every answer carries a `Cache-Control` from the table in `cache-control.ts`,
and `cache-control.test.ts` checks each route and `public/_headers` against
it:

| Answer | `Cache-Control` |
| --- | --- |
| JSON, dashboard pages, redirects and errors | `no-store` (the Worker adds it to any answer without one) |
| `robots.txt` | `public, max-age=3600` |
| `/static/fonts/*` and `/static/assets/*` | `public, max-age=31536000, immutable` |
| `/static/mark.svg` | the platform default, checked before each use |

The dashboard's styles and script live in `public/static/assets/` under a
name that carries the first 12 hex digits of their SHA-256, so a change is a
new URL and browsers may keep each one for a year. There is no build step,
so after editing either file run `npm run hash-assets`: it renames the file
after its new contents and rewrites `dashboard/asset-urls.ts`, which the
pages link through. The tests fail while a name and its contents disagree.

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

See [`docs/server/diagnostics.md`](../../docs/server/diagnostics.md).
