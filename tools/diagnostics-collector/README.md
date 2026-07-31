# Diagnostics collector (Cloudflare Worker → Backblaze B2)

Deployed by **GitHub Actions** ([wrangler-action](https://github.com/cloudflare/wrangler-action)):
[`.github/workflows/diagnostics-collector-deploy.yml`](../../.github/workflows/diagnostics-collector-deploy.yml).

After deploy, `deployment-url` is stored in repository variable
**`DIAGNOSTICS_COLLECTOR_BASE_URL`**.

| Method | Path | Who | Auth |
|--------|------|-----|------|
| `POST` | `/submit` | Bookclerk clients | none (validated + secret heuristics) |
| `GET` | `/report?since=<ms>&after=<name>` | GitHub Action | `Authorization: Bearer <REPORT_API_KEY>` |
| `GET` | `/health` | probes | none |

Each `/submit` is assigned a **`report_id` UUID**; the B2 object is
`diagnostics/<version>/<report_id>.json`. Ingest/Copilot issues list these IDs
for manual review.

### `/report` version filter

Before downloading objects, `/report` keeps only versions that match the latest
**stable** GitHub release for `GITHUB_REPOSITORY` (from `releases/latest`):

- equal to that baseline
- **newer** (semver greater — includes prereleases of a future version)
- **derivative** packaging of the baseline (`1.2.3+nix`, `1.2.3-1`, `1.2.3.fc40`)

If the repo has **no releases** yet (or `releases/latest` is 404), the filter is
disabled and all versions are returned. Ops can override with
`?baseline_version=` (empty = all).

Pagination uses `since` (upload timestamp ms) plus `after` (exclusive object
key within that timestamp) so a truncated batch cannot skip same-ms siblings.

## GitHub secrets

| Secret | Purpose |
|--------|---------|
| `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` | Deploy |
| `DIAGNOSTICS_REPORT_API_KEY` | Worker `REPORT_API_KEY` + ingest |
| `DIAGNOSTICS_B2_*` | B2 → Worker secrets |

### Workflow token permissions

Deploy uses `actions-variables: write` (not broad `actions: write`) so
`GITHUB_TOKEN` can set `DIAGNOSTICS_COLLECTOR_BASE_URL`. Ingest uses
`copilot-requests: write` + `issues: write` per
[gh-aw Copilot auth guidance](https://github.com/github/gh-aw).

## Bookclerk client

Bake the collector URL at build time (CI does this automatically):

```bash
BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL="https://diagnostics.bookclerk.app" \
  cargo build --release -p bookclerk-cli -p bookclerkd -p bookclerk-media-worker
```

Then enable sharing without a config URL:

```toml
[diagnostics]
share_reports = true
```

Override anytime with `collector_url` or runtime `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL`.

## Ingest

[`.github/workflows/diagnostics-ingest.yml`](../../.github/workflows/diagnostics-ingest.yml) reads
`DIAGNOSTICS_COLLECTOR_BASE_URL`.

See [`docs/diagnostics.md`](../../docs/diagnostics.md).
