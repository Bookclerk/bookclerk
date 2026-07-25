# Diagnostics & crash reports

## Operator config

After deploy CI has set `DIAGNOSTICS_COLLECTOR_BASE_URL`, build Bookclerk with that
URL baked in:

```bash
BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL="https://your-worker.workers.dev" cargo build --release -p bookclerk-cli
```

Release builds on `main` in CI pass the repository variable automatically.

```toml
[diagnostics]
share_reports = true
```

Override with `collector_url` or runtime `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL`.

### Local verbosity vs ring buffer

- **stderr / OS facility** (journald, macOS `os_log`, Windows Event Log) honor
  `BOOKCLERK_LOG` → `RUST_LOG` → default `bookclerk=info,warn`.
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
GitHub ingest → Copilot CLI (issues include Report IDs) → Issues
```

## Report IDs

Each `/submit` receives a UUID `report_id`. The B2 object is named
`diagnostics/<version>/<report_id>.json`. Copilot-created issues must list these
IDs so operators can pull the full object from B2 for manual review.

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
| `diagnostics-collector-deploy` | Deploy Worker; set `DIAGNOSTICS_COLLECTOR_BASE_URL` from `deployment-url` |
| `diagnostics-ingest` | Daily `/report` pull + Copilot triage |
| `ci` | Release builds with `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` from repo variable |

### Workflow permissions (least privilege)

Permission keys follow GitHub’s parameterized names (hyphenated in workflow YAML).
See [repository permissions](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#repository-permissions).

| Workflow | Permissions |
|----------|-------------|
| Deploy | `contents: read`, `actions-variables: write` (`gh variable set`) |
| Ingest | `contents: read`, `issues: write`, `copilot-requests: write` |

Ingest Copilot auth follows [GitHub Agentic Workflows](https://github.com/github/gh-aw): prefer
`copilot-requests: write` with `${{ github.token }}` (org billing). Optional
`COPILOT_GITHUB_TOKEN` PAT is a personal-repo / no-central-billing fallback.

### Secrets

`CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `DIAGNOSTICS_REPORT_API_KEY`, `DIAGNOSTICS_B2_*`, optional `COPILOT_GITHUB_TOKEN`.

## Redaction & privacy

Exact-value redaction, upload abort, Worker heuristics, optional `CLIENT_IP_HASH_SALT`.
Uploads include `os`, `arch`, Linux/macOS distro string, and rustc channel/release.

Emails in remote uploads are **partially** masked (not fully `[REDACTED]`):

`address@sub.domain.tld` → `a*****s@***.d****n.tld`

Local part and registrable domain label keep first/last characters; intermediate
subdomain labels become `***`; the TLD is unchanged. Titles, paths, account ids,
and secrets remain fully redacted.
