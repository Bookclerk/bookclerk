# Repository automation

The automation App and the workflows around the main CI run: dependency
updates, security advisories, docs publishing, the website checks (which also
run on pull requests that change the site) and Cloudflare Worker deploys. The
planner, its jobs and `CI Gate` are in [Continuous integration](ci.md).

## Dependabot auto-merge

`.github/workflows/dependabot-auto-merge.yml` runs
[`fastify/github-action-merge-dependabot`](https://github.com/fastify/github-action-merge-dependabot)
when a Dependabot PR opens or updates. It approves the PR and turns on GitHub
auto-merge (squash) **for that PR only**. GitHub then merges it, or adds it to
the merge queue when one is required, once `CI Gate` and `PR scan / osv-scan`
pass. Failing PRs stay open for a human.

The repository-level **Allow auto-merge** setting only makes the per-PR
"Enable auto-merge" button and API available; it never merges a PR on its own,
so human PRs merge exactly as before unless someone opts a PR in.

The action must run while checks are still pending: GitHub rejects enabling
auto-merge on a PR that is already mergeable. To retry a Dependabot PR (for
example after adding the secrets), comment `@dependabot rebase`.

Version updates (`.github/dependabot.yml`, weekly, 7-day cooldown) cover
every lockfile of a shipped artifact or its build: Cargo at the workspace
root, npm in `ui/`, `packages/plugin-sdk`, `tools/diagnostics-collector` and
`tools/site`,
uv in `packages/plugin-sdk-python`, GitHub Actions, and the digest-pinned
base images of `packaging/docker/Dockerfile`. `examples/` lockfiles
are left out (reference plugins, never packaged); the full OSV scan still
reports them.

The Python SDK has no runtime dependencies. Its `uv.lock` pins the `dev`
and `docs` extras that the `python_sdk` check and
`generate-api-docs.sh --python` install (`uv sync --locked --no-install-project`,
which fails when the lockfile is out of date with `pyproject.toml`; the SDK is
imported from `src/` rather than built, so its unlocked build backend never
runs), so OSV scans them and a runtime dependency added later is scanned from
the start. [Docs publishing](#docs-publishing) reads the SDK's source with the
standard library and installs nothing. After
changing `pyproject.toml` dependencies, run `uv lock` in
`packages/plugin-sdk-python`.

Policy knobs live in the workflow's `with:` block:

- `target: any` merges every update type. Tighten to `minor` or `patch` to
  leave larger bumps for manual review.
- `exclude: 'crate-a,crate-b'` skips auto-merge for PRs touching those
  packages.

### Setup

1. **Settings → General → Pull Requests:** enable **Allow auto-merge**.
2. Create a GitHub App (owned by the organization once the repo is
   transferred; an App owned by a user can be transferred later) with
   repository permissions **Contents: read and write** and **Pull requests:
   read and write**, no webhook, and install it on this repository only.
3. Add two **Dependabot** secrets (Settings → Secrets and variables →
   Dependabot). Dependabot-triggered runs do not receive Actions secrets:
   - `DEPENDABOT_MERGE_APP_CLIENT_ID`: the App's client ID.
   - `DEPENDABOT_MERGE_APP_KEY`: a private key generated for the App.

The App token matters: merges and queue entries attributed to `GITHUB_TOKEN`
do not start new workflow runs, so `push` CI on `main`, the OSV SARIF upload,
and `merge_group` checks would be skipped. The App can also approve PRs without
the "Allow GitHub Actions to create and approve pull requests" setting.

### Before the merge queue exists

While the repository is still owned by a user, `Protect Main` keeps "require
branches to be up to date" on. Auto-merge then waits on a Dependabot PR that
has fallen behind `main` until it is rebased. Either:

- turn that option off in the `Protect Main` required-status-checks rule, so
  green Dependabot PRs merge as soon as their checks pass (Dependabot rebases
  PRs that actually conflict, and `push` CI on `main` is the safety net). This
  also relaxes the rule for human PRs until the queue replaces it; or
- leave it on and comment `@dependabot rebase` on PRs that fall behind.

## Automation App

Scheduled workflows that open pull requests or file issues use the same GitHub
App as [Dependabot auto-merge](#dependabot-auto-merge) instead of
`GITHUB_TOKEN`:

- The organization does not allow GitHub Actions to create or approve pull
  requests, so `GITHUB_TOKEN` cannot open them.
- Pushes and pull requests made with `GITHUB_TOKEN` do not start workflow
  runs, so their required checks would never report. App-token pushes start
  CI like any other contributor's.

| Workflow | Writes |
| --- | --- |
| `workerd-pin-bump.yml` | `deps/workerd-pin` branch, pull request, auto-merge |
| `librofm-apk-probe.yml` | `api-drift` issue; `api-drift/librofm` branch and pull request |
| `graphicaudio-api-probe.yml` | `api-drift` issue; `api-drift/graphicaudio` branch and pull request |
| `chirp-api-probe.yml` | `api-drift` issue; `api-drift/chirp` branch and pull request |
| `osv-remediate.yml` | reads OSV code-scanning alerts; `deps/osv-remediation` branch, pull request, auto-merge |
| `cflite-batch.yml` | `fuzz-crash` issues for oracle failures; draft repository security advisories for crashes ([triage](fuzzing.md#fuzz-crash-triage)) |
| `osv-advisories.yml` | reads OSV code-scanning and Dependabot alerts; draft repository security advisories ([verdicts](#osv-advisories)) |
| `docs-publish.yml` | commits and release tags on the `bookclerk-docs` repository ([docs publishing](#docs-publishing)) |

The probes share `.github/actions/api-drift-report`: one open issue per source
(found by a hidden `<!-- api-drift:<source> -->` marker and updated in place,
with a comment per new detection), and, when the probe's fixer produced
verified changes, a draft pull request on a fixed branch that closes it (a
maintainer marks it ready after CI and review). Each
workflow's own `GITHUB_TOKEN` stays read-only, and each job mints the App token
only when it has something to write, scoped to the permissions that step needs.
Commits are made through the API (`sign-commits`), so they are signed and
attributed to the App.

### Setup

1. Give the App these repository permissions, then accept the new
   permissions on its installation (an installation keeps its old
   permissions until they are accepted):
   - **Contents: read and write** (push branches)
   - **Pull requests: read and write** (open, update and auto-merge PRs)
   - **Issues: read and write** (file and update drift issues, add labels)
   - **Code scanning alerts: read** (`osv-remediate.yml` and
     `osv-advisories.yml` read the OSV alerts on `main`)
   - **Dependabot alerts: read** (`osv-advisories.yml`)
   - **Repository security advisories: read and write**
     (`cflite-batch.yml` and `osv-advisories.yml` file draft advisories and
     dedupe against them)
   - **Metadata: read** (always granted)
   - **Variables: read and write**, as an organization and/or repository
     permission (`diagnostics-collector-deploy.yml` stores the workers.dev
     collector URL when `DOMAIN` is unset; organization first, repository as
     the fallback). That token cannot be narrowed, because the token API has
     no Variables permission to request, so the job mints it only for the
     one write.

   **Workflows** is not needed: none of these pull requests touch
   `.github/workflows/`. Install the App on `bookclerk-docs` as well, with
   the same Contents permission, for `docs-publish.yml`.
2. Add two **Actions** secrets (Settings → Secrets and variables → Actions).
   Scheduled runs do not receive the Dependabot copies:
   - `AUTOMATION_APP_CLIENT_ID`: the App's client ID.
   - `AUTOMATION_APP_PRIVATE_KEY`: a private key generated for the App.
3. For `osv-remediate.yml`, add one more Actions secret,
   `OSV_REMEDIATION_BUNDLE_KEY`: a random passphrase (for example
   `openssl rand -base64 32`). It only encrypts the bump while CI tests it;
   rotating it affects nothing but a run already in progress.
4. For `cflite-batch.yml`, add `FUZZ_CRASH_BUNDLE_KEY`, generated the same
   way and kept by maintainers: it seals crash bundles that are read after
   the run. Without it a run that finds a crash fails before uploading
   anything. `osv-advisories.yml` seals its verdict report with the same
   key, and its intermediate evidence with `OSV_REMEDIATION_BUNDLE_KEY`.

### OSV remediation

`osv-remediate.yml` runs after every full OSV scan of `main` (push, schedule
or manual run) and on demand. It reads the open OSV-Scanner alerts in
Security → Code scanning, looks each advisory up on api.osv.dev (with its
aliases; when OSV does not know an alert's rule id, for example a new CVE that
OSV-Scanner chose to name a group of advisories, the other advisory ids in the
alert's rule and message text stand in for it), and
`scripts/osv-remediate.py` tries lockfile-only updates (`cargo update
--precise`, then the newest compatible release; `npm update
--package-lock-only --ignore-scripts`; `uv lock --no-build --upgrade-package
<name>==<fix>`, then the newest allowed release) until no locked version of
the package is inside the advisory's affected ranges. An attempt that does not clear the
advisory is rolled back. Manifests are never edited, so a fix that needs a new
major version, or a change upstream, stays an open alert for a maintainer.

A bump is published only once it is known to pass:

1. `bump` commits the bump locally and seals it, with the pull request body,
   using `OSV_REMEDIATION_BUNDLE_KEY` (`scripts/osv-remediation-bundle.sh`).
   Artifacts of a public repository are downloadable by anyone, so the bump
   never travels in the clear.
2. `verify` calls `ci.yml` with `private-head: true`. Every CI job opens the
   sealed bundle right after checkout (`.github/actions/osv-remediation-head`)
   and tests the bump with the full suite. Caches are not saved, cargo output
   is quiet, and the first failed job cancels the run, as in the merge queue.
3. `publish` runs only when that CI Gate passed. It pushes
   `deps/osv-remediation`, opens the pull request ready for review (the suite
   has already passed, so it skips the draft stage) and turns on auto-merge,
   so the merge queue lands it as soon as the pull request's required checks
   pass.

A bump that fails verification publishes nothing: the run fails and the
alerts stay in Security → Code scanning for a maintainer. A run with nothing
to bump publishes nothing either.

While a remediation pull request is open, later runs stop at the `pending`
job and change nothing. Rebuilding `deps/osv-remediation` on every push to
`main` would restart the pull request's checks and drop it from the merge
queue, and the queue already tests it against the latest `main`. Once it
merges or is closed, the next full scan of `main` starts a fresh run. To
replace an open remediation pull request that failed its checks, close it and
run the workflow manually.

Scan results are not public, so the run keeps them private: its log prints
counts only (command output is discarded, because a failed attempt would name
a vulnerable package), and the pull request lists only the bumps it applied
with their advisory ids. This hides which advisories the project is acting on
until the fix is ready to land. It cannot hide the advisories themselves:
`main`'s lockfiles are public, so anyone can scan them.

Dependabot does not cover this: it raises alerts and security updates only for
advisories GitHub has reviewed, and RustSec advisories often stay unreviewed,
while its version updates wait out the cooldown and never touch transitive-only
crates.

Pull requests the App opens are not authored by a person, so rulesets that
require approval for such changes still need a maintainer's approval before
the merge queue takes them. Auto-merge stays on while it waits.

### OSV advisories

`osv-advisories.yml` runs after every full OSV scan of `main`, next to the
remediation, and answers a different question: does an open dependency alert
affect something Bookclerk ships? A draft repository security advisory is
filed only when it does ([ADR](../adr/security-advisory-automation.md)). Drafts
are private to maintainers and are filed before the first release too, which
proves the path; nothing becomes public until a maintainer publishes.

[`shipped-artifacts.toml`](../../shipped-artifacts.toml) declares the shipped
artifacts and which lockfile entries end up in each; every lockfile in the
repository is either listed there or named as not shipped
(`scripts/tests/test_osv_advisories.py` fails on an unclassified one). For
each open OSV-Scanner alert and Dependabot alert on `main` (merged per
lockfile when their ids or aliases overlap), `scripts/osv-advisories.py`
gives every artifact of that lockfile a verdict:

| Lockfile | Artifact | Affected when |
| --- | --- | --- |
| `Cargo.lock` | host binaries, platform and optional guests | the crate is in the shipped closure of the shipped crates in `cargo metadata` (normal edges, every target platform; dev and build dependencies and proc macros excluded, except `*-src` build dependencies, which compile vendored C into the binary; their own dependencies, such as `cc`, are build tooling and excluded), and, when the upstream advisory names vulnerable functions, one of them is in the DWARF of the fat-LTO release build of the shipped binaries (the method of OSV-Scanner's Rust call analysis) |
| `Cargo.lock` | Rust plugin SDK | the crate is in the SDK's shipped closure (a library: no binary to analyze) |
| `ui/package-lock.json` | web UI, served by `bookclerkd` | a production dependency with code in the production bundle (`vite build --sourcemap`, then the source maps); if the bundle cannot be built, any production dependency. A test keeps the UI from importing package files as `?url`/`?raw` assets or wasm, which source maps would not show |
| `packages/plugin-sdk/package-lock.json` | `@bookclerk/plugin-sdk` | a production dependency (not `dev` in the lockfile) |
| `tools/diagnostics-collector/package-lock.json` | diagnostics collector Worker | a production dependency (it has none today) |
| `packages/plugin-sdk-python/` | `bookclerk-plugin-sdk` (PyPI) | never by a dependency: the SDK has no runtime dependencies, and both the test and the workflow fail if `[project].dependencies` or a non-dev extra gains one before its closure is resolved |
| anything else | none | never: examples, fuzzing, scripts and dev dependencies stay an alert plus the remediation bump |

The call-analysis build takes hours, so it runs only when an advisory with
function data for a shipped crate has no advisory yet or only an open draft
(which the analysis then refreshes), and then only on the weekly scheduled
scan or a manual run (`call-analysis` input forces it). On other runs such an
advisory waits as "awaiting call analysis". Runs queue one at a time, and a
newer run replaces one still waiting, so a scheduled run queued behind
another can give way to a push's run; its call analysis then waits a week,
or a manual run with `call-analysis` does it now. An open draft whose
advisory the analysis (or the web bundle) later shows unaffected is left as
is and reported as "open draft, now not affected" for a maintainer to close.
The web UI bundle is built only when an open advisory is on
`ui/package-lock.json`.
A build that fails, reports no executables, or leaves a binary with too few
functions or no `main` in its DWARF fails closed: the advisory is filed on
the closure alone. An advisory a maintainer judges unaffected is closed as a
draft (it is not filed again) or ignored in `osv-scanner.toml`.

A draft carries the upstream ids (each as a hidden
`<!-- osv-advisory: <id> <ecosystem>/<package> -->` marker, so it is never
filed twice and a draft a maintainer closed is not filed again), the locked
and first fixed versions, the severity (a CVSS 3.1 vector from the GHSA
record or the RustSec entry, else the highest GHSA severity, else low for an
informational notice such as "unmaintained" and medium otherwise), CWE ids
(CWE-1395 for every draft, the GHSA records' CWE ids, and CWEs mapped from
RustSec categories and informational kinds, which RustSec records carry
instead), the verdict and its evidence
(saying so when reachability is unproven), and a proposed range up to the
highest release tag (or, for the TypeScript and Python SDKs, the version in
their manifest) for a maintainer to confirm. The affected package's
ecosystem is the one `shipped-artifacts.toml` declares (`other` for
packages on no public registry), except that an artifact with a
`crate_manifest` is filed as `rust` once that crate's `publish` allows
crates.io; markers keep the declared ecosystem. A later run that finds the same advisory never files
a second draft: while the first is still a draft it rewrites only the
automated part of its description (between the
`<!-- bookclerk-auto: begin … -->` and `<!-- bookclerk-auto: end -->`
comments: new evidence, fixed versions, aliases, the newest release), adds
markers for new aliases, and fills an affected range that is still unset
once a release exists. Text a maintainer adds outside that part, and the
severity, CWE ids, summary and any range a maintainer set, are left alone;
nothing is rewritten when only the "Last seen" line would change; closed and
published advisories are never touched. One run files at most ten drafts
(and updates at most ten); a draft GitHub
refuses is reported and fails the run without holding back the others.
Before the first release no draft proposes a range (nothing is published
yet): the draft says so, and a maintainer either closes it once the fix is
on `main` or sets the range when a release ships the code.

Three jobs keep secrets away from third-party build code:

1. `fetch` mints a read-only App token (code scanning, Dependabot and
   repository advisories) for one step that reads the alerts, their records
   on api.osv.dev, the newest release and the advisories on record, and
   revokes it when the step ends. It then computes the closures from the
   lockfiles and `cargo metadata`, which runs no build script, and decides on
   call analysis. Its output leaves the job sealed with
   `OSV_REMEDIATION_BUNDLE_KEY`.
2. `analyze` has no secrets and no token. It builds the web UI
   (`npm ci --ignore-scripts`) and, when planned, the shipped binaries. Its
   output names no advisory (the bundled packages and a truncated SHA-256 of
   every function in each binary), so it travels unsealed.
3. `file` builds nothing. It mints an advisory-write token, files the
   drafts, and writes the verdict report.

The verdict report is the record for everything that did not become a draft:
one row per (advisory, shipped artifact) with its verdict, packages and
outcome (filed, already on record, capped, failed), the advisories on
lockfiles that do not ship, and the evidence behind each verdict. It never
leaves the job in the clear: it is sealed with `FUZZ_CRASH_BUNDLE_KEY` (the
passphrase maintainers keep) and uploaded as the `osv-advisories-report`
artifact of that workflow run, kept 30 days. Anyone can download the sealed
file from the run page (Actions → OSV advisories → the run → Artifacts) or
with `gh run download <run-id> -n osv-advisories-report`; only the
passphrase opens it:
`BUNDLE_KEY=<passphrase> scripts/osv-remediation-bundle.sh open sealed.bin <dir>`
then read `<dir>/report.md`.

Logs print counts only. A new shipped artifact, or a new lockfile, goes into
`shipped-artifacts.toml` in the pull request that adds it. Cutting the fix
release automatically is a documented future goal, not built yet (see the
ADR).

### Docs publishing

`docs-publish.yml` keeps the separate public repository
[`Bookclerk/bookclerk-docs`](https://github.com/Bookclerk/bookclerk-docs) in
step with this one, as Markdown that the website and agents read:

- Its `main` mirrors this repository's `main`. Every push to `main` builds
  the tree and commits it there with a `Source-Commit: <sha>` trailer naming
  the commit it was built from. Runs on `main` share one concurrency group
  that cancels the older run, so a burst of merges publishes only the newest commit, and a
  run whose commit is no longer the tip of `main` when it reaches the publish
  step skips it. A build identical to the docs `main` adds no commit.
- A published release tags the docs commit whose trailer names the release's
  commit with the same tag name. When no `main` commit matches (a burst
  skipped it), the release's build is committed beside `main`, parented on
  its head without moving it, and that commit is tagged. An existing tag is
  never moved.
- The tree is `docs/` as it is, plus `api/`, whose index lists the plugin
  SDKs first (TypeScript, Python, Rust), then the web API, then the Rust
  crates and the web UI library:
  - TypeScript: TypeDoc's Markdown theme for `@bookclerk/plugin-sdk` and
    `ui/src/lib` (`typedoc-plugin-markdown`, a dev dependency of both), one
    page per module, without source links or breadcrumbs.
  - Python: `scripts/python_api_markdown.py` reads the SDK with `ast` (it
    imports nothing) and renders each public module's `__all__`, with
    Google-style sections as lists and cross-references as links.
  - Rust: `scripts/rustdoc_markdown.py` renders rustdoc JSON for every
    workspace library from a pinned nightly, one page per public module.
    Items re-exported from private modules are documented where they are
    re-exported, signatures are rebuilt from the JSON, doc headings sit
    under their item, and intra-doc links come from rustdoc's own link
    table. `api/rust/README.md` groups the crates (plugin interface,
    plugins, host internals, development).
  - Web API: `scripts/openapi_markdown.py` renders `bookclerkd`'s committed
    OpenAPI document (`crates/bookclerkd/openapi.json`) as an index of
    routes and credentials, one page per route group with each route's
    access rule, parameters, body and responses, and a page of schemas.

  Every relative link to a file in the tree must resolve, and links in
  `api/` must stay inside the tree, or the build fails.
- `last-updated.json` at the root: the date (UTC, `YYYY-MM-DD`) each page
  of `docs/` last changed, which the website shows as "Last updated". It
  is the author date of the newest commit that changed the page's content.
  A pure move (a rename git sees as unchanged) is followed back to the old
  path rather than counted, and author dates survive history rewrites that
  reset commit dates. The workflow checks out the full history and passes
  `--require-history`, so a shallow checkout fails the build; a local build
  from a shallow clone leaves the file out with a warning.

The `build` job has no secrets and a read-only token; it runs
`scripts/docs-markdown.py` and uploads the tree as an artifact. The `publish`
job checks out only `scripts/docs-publish.py` from the default branch, mints
the [Automation App](#automation-app) token for `bookclerk-docs` alone
(contents: write), and writes one commit through the Git Data API, so GitHub
signs it as the App. A manual run publishes only from `main`. The publish
script refuses redirects, so the token is only sent to `api.github.com`.

`docs-markdown.py` clears its output before each build, so it refuses a
scratch root (`--scratch-root`, default `target/docs-markdown/`) that is not
strictly inside the repository's `target/` or the system temp directory, an
output directory that is not strictly inside the scratch root, and an
existing output directory that lacks the marker file it writes. The filesystem root, the
home directory, the repository root, `/mnt` and `/tmp` are refused as either
the output or the scratch root.

To build the tree locally (it lands in `target/docs-markdown/out/`):

```bash
rustup toolchain install nightly-2025-11-15 --profile minimal
python3 scripts/docs-markdown.py
```

The nightly is pinned because rustdoc's JSON format changes between
nightlies and `rustdoc_markdown.py` reads format 56; bump `RUST_NIGHTLY` and
`RUST_JSON_FORMAT` in `docs-markdown.py` and the workflow together, and adjust
the renderer for any change in the format. The nightly may predate the workspace's
`rust-version`, so the build passes `--ignore-rust-version`; it only produces
documentation.

### Website checks

`site_audit` (`tools/site/audit/audit.ts`) builds the site, serves it
through the real Worker under `wrangler dev`, and audits it with the
runner's Chrome. Docs pages are read from this checkout's `docs/` through a
local server (`tools/site/audit/entry.ts`), not from the docs repository,
so the audit covers the docs as the pull request has them and needs no
network. A docs key the local server fails renders the docs-unavailable page.

| Pages | Lighthouse (mobile and desktop) |
| --- | --- |
| `/`, `/download`, `/docs/guide/getting-started`, `/docs/contributing/ci` | performance ≥ 90, accessibility 100, best practices ≥ 95, SEO 100, CLS ≤ 0.05 |
| 404 and docs-unavailable | accessibility 100 (their SEO and best-practice misses, such as the status code and `noindex`, are intended) |

A run that misses only on performance is repeated once and the better run
kept, since lab scores vary on shared runners. axe-core then checks every
page in light and dark color schemes at 1350 and 390 px against WCAG 2.0 to
2.2 A/AA and its best-practice rules, and any violation fails the check. The
results table goes to the job summary; a failed audit uploads the Lighthouse
reports as the `site-audit` artifact. Locally: `npm run build` in
`tools/site`, then `npm ci && node audit.ts` in `tools/site/audit`
(`--chrome <path>` or `CHROME_PATH` picks the browser).

## Cloudflare Worker deploys

Three workflows deploy Workers with `wrangler deploy`, sharing the repository
secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`:

| Workflow | Worker | Custom domain |
|----------|--------|---------------|
| `site-deploy.yml` | `bookclerk-site` | `<DOMAIN>` |
| `diagnostics-collector-deploy.yml` | `bookclerk-diagnostics` | `diagnostics.<DOMAIN>` |
| `demo-deploy.yml` | `bookclerk-demo` | `demo.<DOMAIN>` |

`DOMAIN` is the organization variable (default `bookclerk.app` for the site). With `DOMAIN` unset the diagnostics collector and the demo are served on the account's workers.dev subdomain instead (see `docs/server/diagnostics.md` and `tools/demo/README.md`).

The demo deploy builds the production builder and image from the deployed
commit, then lets `wrangler deploy` build `packaging/demo/Dockerfile` on top
of them and push it to Cloudflare's container registry. It runs when the demo
changes on `main`, when a release is published, nightly, and on demand. A
nightly or release run first asks the live demo's `/_demo/health` which commit
it runs and stops before building when it is the same one and an earlier
run for that commit finished successfully (the Worker reports its version
before migrations and smoke checks, so a half-finished deploy is retried). A
repository without the two Cloudflare secrets gets a notice instead of a
failure.

The API token is scoped to the Workers by name, so it needs, for each Worker
the workflows deploy:

- The **Workers** permission with the **Editor** role, scoped to
  *Individual Workers* and naming the Worker (`bookclerk-site`,
  `bookclerk-diagnostics`, `bookclerk-demo`). This is the current Workers permission; the
  legacy *Workers Scripts: Edit* maps to Editor on all Workers, which is
  broader than the deploys need.
- **Zone → Workers Routes → Edit** (Write in the API reference) on the
  `<DOMAIN>` zone, for the custom domain.
- For `bookclerk-diagnostics` only, **Account → Workers KV Storage → Edit**
  for the deploy that first creates its `TRIAGE_BUDGET` namespace (the
  binding has no id, so wrangler creates it). Deploying bindings needs only
  Editor on the Worker, so this can be removed once the namespace exists.
- For `bookclerk-diagnostics` only, **Account → D1 → Edit**: the first
  deploy creates the dashboard's `DASHBOARD_DB` database (no id in the
  binding), and every deploy applies its migrations with
  `wrangler d1 migrations apply`, so this one stays.
- For `bookclerk-demo` only: **Account → D1 → Edit** for the same reasons
  (its `DB` database); **Account → Containers → Edit**, since every deploy
  pushes the image and updates the container application; and
  **Account → Turnstile → Edit**, since every deploy finds or creates the
  `bookclerk-demo` widget, keeps its hostnames current and reads its keys
  (`tools/demo/scripts/worker-secrets.ts`).

Editor cannot create a Worker; creating one takes the **Admin** role on the
Workers product. So the first deploy of a new Worker needs one of these:

1. Create the Worker in the Cloudflare dashboard first, under its exact name
   (an empty "Hello World" Worker is fine; the first deploy replaces it), then
   add it to the token's Individual Workers scope; or
2. Temporarily give the token the Workers Admin role, run the first deploy,
   then remove it again and add the new Worker to the Individual Workers
   scope.

See [Workers roles and permissions](https://developers.cloudflare.com/workers/authorization/workers/).

After that, the two permissions above are enough, plus the per-Worker
permissions listed for `bookclerk-diagnostics` and `bookclerk-demo` that
every deploy uses (D1 for both; Containers and Turnstile for the demo).
Renaming a Worker in
`wrangler.toml` creates a new Worker, so it repeats this step and leaves the
old one behind; keep the names as they are.
