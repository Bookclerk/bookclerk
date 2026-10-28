# Repository automation

This page explains the automation App and the workflows around the main CI
run: dependency updates, security advisories, docs publishing, the website
checks and the Cloudflare Worker deploys.

The website checks also run on pull requests that change the site. The
planner, its jobs and `CI Gate` are in [Continuous integration](ci.md).

## Dependabot auto-merge

`.github/workflows/dependabot-auto-merge.yml` runs
[`fastify/github-action-merge-dependabot`](https://github.com/fastify/github-action-merge-dependabot)
when a Dependabot pull request opens or updates. It approves the pull
request and turns on GitHub auto-merge (squash) **for that pull request
only**.

GitHub then merges it, or adds it to the merge queue when one is required,
once `CI Gate` and `PR scan / osv-scan` pass. Failing pull requests stay
open for a human.

The repository-level **Allow auto-merge** setting only makes the
per-pull-request "Enable auto-merge" button and API available. It never
merges a pull request on its own, so human pull requests merge exactly as
before unless someone opts one in.

The action must run while checks are still pending, because GitHub rejects
enabling auto-merge on a pull request that is already mergeable. To retry a
Dependabot pull request (for example, after adding the secrets), comment
`@dependabot rebase`.

### What Dependabot updates

Version updates (`.github/dependabot.yml`, weekly, with a 7-day cooldown)
cover every lockfile of a shipped artifact or its build:

- Cargo at the workspace root;
- npm in `ui/`, `packages/plugin-sdk`, `tools/diagnostics-collector`,
  `tools/demo` and `tools/site`;
- uv in `packages/plugin-sdk-python`;
- GitHub Actions;
- the digest-pinned base images of `packaging/docker/Dockerfile`.

`examples/` lockfiles are left out, because they are reference plugins and
never packaged. The full OSV scan still reports them.

The Python SDK has no runtime dependencies. Its `uv.lock` pins the `dev`
and `docs` extras that the `python_sdk` check and
`generate-api-docs.sh --python` install, so OSV scans them, and a runtime
dependency added later is scanned from the start.

- The install is `uv sync --locked --no-install-project`, which fails when
  the lockfile is out of date with `pyproject.toml`.
- The SDK is imported from `src/` rather than built, so its unlocked build
  backend never runs.
- [Docs publishing](#docs-publishing) reads the SDK's source with the
  standard library and installs nothing.

After changing `pyproject.toml` dependencies, run this in
`packages/plugin-sdk-python`:

```sh
uv lock
```

### Policy knobs

The knobs live in the workflow's `with:` block:

- **`target: any`** merges every update type. Tighten it to `minor` or
  `patch` to leave larger bumps for manual review.
- **`exclude: 'crate-a,crate-b'`** skips auto-merge for pull requests
  touching those packages.

### Setup

1. In **Settings → General → Pull Requests**, enable **Allow auto-merge**.
2. Create a GitHub App with repository permissions **Contents: read and
   write** and **Pull requests: read and write**, and no webhook. Install
   it on this repository only. Transferring the repository doesn't
   transfer the App: if the organization should own it, transfer the App
   separately.
3. Add two **Dependabot** secrets (**Settings → Secrets and variables →
   Dependabot**), because Dependabot-triggered runs don't receive Actions
   secrets:
   - `DEPENDABOT_MERGE_APP_CLIENT_ID`: the App's client ID.
   - `DEPENDABOT_MERGE_APP_KEY`: a private key generated for the App.

The App token matters. Merges and queue entries attributed to
`GITHUB_TOKEN` don't start new workflow runs, so `push` CI on `main`, the
OSV SARIF upload and the `merge_group` checks would be skipped. The App can
also approve pull requests without the "Allow GitHub Actions to create and
approve pull requests" setting.

### Before the merge queue exists

While the repository is still owned by a user, `Protect Main` keeps
"require branches to be up to date" on. Auto-merge then waits on a
Dependabot pull request that has fallen behind `main` until it is rebased.
Either:

- turn that option off in the `Protect Main` required-status-checks rule,
  so green Dependabot pull requests merge as soon as their checks pass.
  Dependabot rebases pull requests that actually conflict, and `push` CI on
  `main` is the safety net. This also relaxes the rule for human pull
  requests until the queue replaces it; or
- leave it on, and comment `@dependabot rebase` on pull requests that fall
  behind.

## Automation App

Scheduled workflows that open pull requests or file issues use the same
GitHub App as [Dependabot auto-merge](#dependabot-auto-merge), not
`GITHUB_TOKEN`:

- The organization doesn't allow GitHub Actions to create or approve pull
  requests, so `GITHUB_TOKEN` can't open them.
- Pushes and pull requests made with `GITHUB_TOKEN` don't start workflow
  runs, so their required checks would never report. App-token pushes
  start CI like any other contributor's.

| Workflow | Writes |
| --- | --- |
| `workerd-pin-bump.yml` | The `deps/workerd-pin` branch, pull request and auto-merge |
| `librofm-apk-probe.yml` | An `api-drift` issue; the `api-drift/librofm` branch and pull request |
| `graphicaudio-api-probe.yml` | An `api-drift` issue; the `api-drift/graphicaudio` branch and pull request |
| `chirp-api-probe.yml` | An `api-drift` issue; the `api-drift/chirp` branch and pull request |
| `osv-remediate.yml` | Reads OSV code-scanning alerts; the `deps/osv-remediation` branch, pull request and auto-merge |
| `cflite-batch.yml` | `fuzz-crash` issues for oracle failures; draft security advisories for crashes ([triage](fuzzing.md#fuzz-crash-triage)) |
| `osv-advisories.yml` | Reads OSV code-scanning and Dependabot alerts; draft security advisories ([verdicts](#osv-advisories)) |
| `docs-publish.yml` | Commits and release tags on the `bookclerk-docs` repository ([docs publishing](#docs-publishing)) |
| `cla.yml` | Signature commits on the `cla-signatures` branch, the only writer its ruleset allows; the check's pull request comment and `Bookclerk CLA` check run ([CLA check](licensing.md#the-cla-check)) |

The probes share `.github/actions/api-drift-report`:

- One open issue per source, found by a hidden
  `<!-- api-drift:<source> -->` marker and updated in place, with a comment
  per new detection.
- When the probe's fixer produced verified changes, a draft pull request on
  a fixed branch that closes the issue. A maintainer marks it ready after
  CI and review.

Each workflow's own `GITHUB_TOKEN` stays read-only. Each job mints the App
token only when it has something to write, scoped to the permissions that
step needs. Commits are made through the API (`sign-commits`), so they are
signed and attributed to the App.

### Setup

1. Give the App these repository permissions, then accept the new
   permissions on its installation (an installation keeps its old
   permissions until they are accepted):
   - **Contents: read and write**, to push branches.
   - **Pull requests: read and write**, to open, update and auto-merge
     pull requests.
   - **Checks: read and write**, for the `Bookclerk CLA` check run
     (`cla.yml`), which GitHub lists under the App's name and the logo
     set in the App's **Display information**.
   - **Commit statuses: read and write**, so `cla.yml` can update a
     `Bookclerk CLA` commit status left on a commit from before the check
     became a check run.
   - **Issues: read and write**, to file and update drift issues and add
     labels.
   - **Code scanning alerts: read**, because `osv-remediate.yml` and
     `osv-advisories.yml` read the OSV alerts on `main`.
   - **Dependabot alerts: read**, for `osv-advisories.yml`.
   - **Repository security advisories: read and write**, because
     `cflite-batch.yml` and `osv-advisories.yml` file draft advisories and
     deduplicate against them.
   - **Metadata: read** (always granted).
   - **Variables: read and write**, as an organization or repository
     permission, or both. `diagnostics-collector-deploy.yml` stores the
     workers.dev collector URL when `DOMAIN` is unset, organization first
     and repository as the fallback. That token can't be narrowed, because
     the token API has no Variables permission to request, so the job mints
     it only for the one write.

   > [!NOTE]
   > Grant and accept a permission before merging a workflow change that
   > requests it. A job that asks for a permission the installation lacks
   > fails to mint its token, so `cla.yml`, for one, would report no
   > **Bookclerk CLA** check at all and block every pull request.

   **Workflows** isn't needed, because none of these pull requests touch
   `.github/workflows/`. Install the App on `bookclerk-docs` as well, with
   the same Contents permission, for `docs-publish.yml`.
2. Add two **Actions** secrets (**Settings → Secrets and variables →
   Actions**). Scheduled runs don't receive the Dependabot copies.
   - `AUTOMATION_APP_CLIENT_ID`: the App's client ID.
   - `AUTOMATION_APP_PRIVATE_KEY`: a private key generated for the App.
3. For `osv-remediate.yml`, add one more Actions secret,
   `OSV_REMEDIATION_BUNDLE_KEY`: a random passphrase (for example, from
   `openssl rand -base64 32`). It only encrypts the bump while CI tests it,
   so rotating it affects nothing but a run already in progress.
4. For `cflite-batch.yml`, add `FUZZ_CRASH_BUNDLE_KEY`, generated the same
   way and kept by maintainers. It seals crash bundles that are read after
   the run. Without it, a run that finds a crash fails before uploading
   anything.

`osv-advisories.yml` seals its verdict report with `FUZZ_CRASH_BUNDLE_KEY`
too, and its intermediate evidence with `OSV_REMEDIATION_BUNDLE_KEY`.

## OSV remediation

`osv-remediate.yml` runs after every successful full OSV scan of `main`
(push, schedule or manual run). It also runs on demand from `main`,
without a preceding scan. It reads the open OSV-Scanner
alerts in **Security → Code scanning** and looks each advisory up on
api.osv.dev, with its aliases.

When OSV doesn't know an alert's rule id, the other advisory ids in the
alert's rule and message text stand in for it. That happens, for example,
with a new CVE that OSV-Scanner chose to name a group of advisories.

`scripts/osv-remediate.py` then tries lockfile-only updates until no locked
version of the package is inside the advisory's affected ranges:

- **Cargo**: `cargo update --package <name>@<locked> --precise <fix>`,
  then the newest compatible release.
- **npm**: `npm update <name> --package-lock-only --ignore-scripts`.
- **uv**: `uv lock --no-build --upgrade-package <name>==<fix>`, then the
  newest allowed release.

`<name>` is the package, `<locked>` its locked version and `<fix>` the
smallest fixed version above it.

An attempt that doesn't clear the advisory is rolled back. Manifests are
never edited, so a fix that needs a new major version, or a change
upstream, stays an open alert for a maintainer.

A bump is published only once it is known to pass:

1. **`bump`** commits the bump locally and seals it, with the pull request
   body, using `OSV_REMEDIATION_BUNDLE_KEY`
   (`scripts/osv-remediation-bundle.sh`). Anyone can download the
   artifacts of a public repository, so the bump never travels in the
   clear.
2. **`verify`** calls `ci.yml` with `private-head: true`. Every CI job
   opens the sealed bundle right after checkout
   (`.github/actions/osv-remediation-head`) and tests the bump with the
   full suite. As in the merge queue, caches aren't saved, cargo output is
   quiet, and the first failed job cancels the run.
3. **`publish`** runs only when that `CI Gate` passed. It pushes
   `deps/osv-remediation` and opens the pull request ready for review,
   skipping the draft stage because the suite has already passed. It turns
   on auto-merge, so the merge queue lands it as soon as the pull request's
   required checks pass.

A bump that fails verification publishes nothing. The run fails, and the
alerts stay in **Security → Code scanning** for a maintainer. A run with
nothing to bump publishes nothing either.

While a remediation pull request is open, later runs stop at the `pending`
job and change nothing. Rebuilding `deps/osv-remediation` on every push to
`main` would restart the pull request's checks and drop it from the merge
queue, and the queue already tests it against the latest `main`.

- Once it merges or is closed, the next full scan of `main` starts a fresh
  run.
- To replace an open remediation pull request that failed its checks,
  close it and run the workflow manually.

Scan results aren't public, so the run keeps them private:

- Its log prints counts only. Command output is discarded, because a
  failed attempt would name a vulnerable package.
- The pull request lists only the bumps it applied, with their advisory
  ids.

That hides which advisories the project is acting on until the fix is
ready to land. It can't hide the advisories themselves: `main`'s lockfiles
are public, so anyone can scan them.

Dependabot doesn't cover this. It raises alerts and security updates only
for advisories GitHub has reviewed, and RustSec advisories often stay
unreviewed. Its version updates wait out the cooldown and never touch
transitive-only crates.

> [!NOTE]
> Pull requests the App opens aren't authored by a person. Rulesets that
> require approval for such changes still need a maintainer's approval
> before the merge queue takes them. Auto-merge stays on while it waits.

## OSV advisories

`osv-advisories.yml` runs after every successful full OSV scan of `main`,
next to the remediation, and on demand from `main`. It answers a different question: does an open dependency
alert affect something Bookclerk ships?

A draft repository security advisory is filed only when it does (see the
[ADR](../adr/security-advisory-automation.md)). Drafts are private to
maintainers, and are filed before the first release too, which proves the
path. Nothing becomes public until a maintainer publishes.

### Verdicts

[`shipped-artifacts.toml`](../../shipped-artifacts.toml) declares the
shipped artifacts, and which lockfile entries end up in each. Every
lockfile in the repository is either listed there or named as not shipped;
`scripts/tests/test_osv_advisories.py` fails on an unclassified one.

`scripts/osv-advisories.py` takes each open OSV-Scanner alert and
Dependabot alert on `main`, merged per lockfile when their ids or aliases
overlap. It gives every artifact of that lockfile a verdict:

| Lockfile | Artifact |
| --- | --- |
| `Cargo.lock` | Host binaries, platform and optional guests |
| `Cargo.lock` | The Rust plugin SDK |
| `ui/package-lock.json` | The web UI, served by `bookclerkd` |
| `packages/plugin-sdk/package-lock.json` | `@bookclerk/plugin-sdk` |
| `tools/diagnostics-collector/package-lock.json` | The diagnostics collector Worker |
| `tools/demo/package-lock.json` | The hosted demo |
| `packages/plugin-sdk-python/uv.lock` | `bookclerk-plugin-sdk` (PyPI) |
| Anything else | None |

An artifact is affected when:

- **Host binaries and guests**: the crate is in the shipped closure of the
  shipped crates in `cargo metadata`. When the upstream advisory names
  vulnerable functions, one of them must also be in the DWARF of the
  fat-LTO release build of the shipped binaries (the method of
  OSV-Scanner's Rust call analysis).
- **The Rust plugin SDK**: the crate is in the SDK's shipped closure. It is
  a library, so there is no binary to analyze.
- **The web UI**: a production dependency has code in the production
  bundle (`vite build --sourcemap`, then the source maps). If the bundle
  can't be built, any production dependency counts.
- **`@bookclerk/plugin-sdk`**: a production dependency (not `dev` in the
  lockfile).
- **The diagnostics collector**: a production dependency. It has none
  today.
- **The Python SDK**: never by a dependency, because the SDK has no runtime
  dependencies. Both the test and the workflow fail if
  `[project].dependencies` or a non-dev extra gains one before its closure
  is resolved.
- **Anything else**: never. Examples, fuzzing, scripts and dev dependencies
  stay an alert plus the remediation bump.

The shipped Rust closure follows normal edges on every target platform. It
excludes dev and build dependencies and proc macros, except `*-src` build
dependencies, which compile vendored C into the binary. Their own
dependencies, such as `cc`, are build tooling and are excluded.

A test keeps the UI from importing package files as `?url` or `?raw`
assets, or as wasm, which the source maps wouldn't show.

### Call analysis

The call-analysis build takes hours. So it runs only when an advisory with
function data for a shipped crate has no advisory yet, or only an open
draft (which the analysis then refreshes). Even then, it runs only on the
weekly scheduled scan or a manual run; the `call-analysis` input forces
it.

- On other runs, such an advisory waits as "awaiting call analysis".
- Runs queue one at a time, and a newer run replaces one still waiting. So
  a scheduled run queued behind another can give way to a push's run. Its
  call analysis then waits a week, or a manual run with `call-analysis`
  does it now.
- An open draft whose advisory the analysis (or the web bundle) later shows
  unaffected is left as it is. It is reported as "open draft, now not
  affected" for a maintainer to close.

The web UI bundle is built only when an open advisory is on
`ui/package-lock.json`.

A build that fails, reports no executables, or leaves a binary with too
few functions or no `main` in its DWARF fails closed: the advisory is filed
on the closure alone. An advisory a maintainer judges unaffected is closed
as a draft (it isn't filed again) or ignored in `osv-scanner.toml`.

### What a draft carries

- **Upstream ids**, each as a hidden
  `<!-- osv-advisory: <id> <ecosystem>/<package> -->` marker, so it is
  never filed twice and a draft a maintainer closed isn't filed again.
- **The locked and first fixed versions.**
- **Severity**: a CVSS 3.1 vector from the GHSA record or the RustSec
  entry, else the highest GHSA severity. Failing both, low for an
  informational notice such as "unmaintained", and medium otherwise.
- **CWE ids**: CWE-1395 for every draft, the GHSA records' CWE ids, and
  CWEs mapped from RustSec categories and informational kinds, which
  RustSec records carry instead.
- **The verdict and its evidence**, saying so when reachability is
  unproven.
- **A proposed range** up to the highest release tag, for a maintainer to
  confirm. For the TypeScript and Python SDKs, it runs up to the version in
  their manifest.

The affected package's ecosystem is the one `shipped-artifacts.toml`
declares (`other` for packages on no public registry). The exception is an
artifact with a `crate_manifest`, which is filed as `rust` once that
crate's `publish` allows crates.io. Markers keep the declared ecosystem.

### Later runs

A later run that finds the same advisory never files a second draft. While
the first is still a draft, the run:

- rewrites only the automated part of its description, between the
  `<!-- bookclerk-auto: begin … -->` and `<!-- bookclerk-auto: end -->`
  comments (new evidence, fixed versions, aliases, the newest release);
- adds markers for new aliases;
- fills an affected range that is still unset, once a release exists.

Text a maintainer adds outside that part is left alone, and so are the
severity, CWE ids, summary and any range a maintainer set. Nothing is
rewritten when only the "Last seen" line would change. Closed and
published advisories are never touched.

One run files at most ten drafts and updates at most ten. A draft GitHub
refuses is reported, and fails the run without holding back the others.

Before the first release, no draft proposes a range, because nothing is
published yet. The draft says so, and a maintainer either closes it once
the fix is on `main`, or sets the range when a release ships the code.

### Jobs and the verdict report

Three jobs keep secrets away from third-party build code:

1. **`fetch`** mints a read-only App token (code scanning, Dependabot and
   repository advisories) for one step. That step reads the alerts, their
   records on api.osv.dev, the newest release and the advisories on
   record, then revokes the token. The job then computes the closures from
   the lockfiles and `cargo metadata` (which runs no build script), and
   decides on call analysis. Its output leaves the job sealed with
   `OSV_REMEDIATION_BUNDLE_KEY`.
2. **`analyze`** has no secrets and no token. It builds the web UI
   (`npm ci --ignore-scripts`) and, when planned, the shipped binaries. Its
   output names no advisory (the bundled packages and a truncated SHA-256
   of every function in each binary), so it travels unsealed.
3. **`file`** builds nothing. It mints an advisory-write token, files the
   drafts and writes the verdict report.

The verdict report records every decision:

- one row per advisory and shipped artifact, with its verdict, packages
  and outcome (filed, already on record, capped, failed);
- the advisories on lockfiles that don't ship;
- the evidence behind each verdict.

It never leaves the job in the clear. It is sealed with
`FUZZ_CRASH_BUNDLE_KEY` (the passphrase maintainers keep) and uploaded as
the `osv-advisories-report` artifact of that workflow run, kept 30 days.

To read it:

1. Download the sealed file from the run page (**Actions → OSV advisories
   → the run → Artifacts**), or with the GitHub CLI:

   ```sh
   gh run download <run-id> -n osv-advisories-report
   ```

   Replace `<run-id>` with the number in the run page's URL.

2. Open it with the passphrase:

   ```sh
   BUNDLE_KEY=<passphrase> scripts/osv-remediation-bundle.sh open sealed.bin <dir>
   ```

   Replace `<passphrase>` with the bundle key maintainers keep, and `<dir>`
   with a new folder to open the report into. `sealed.bin` is the file the
   download writes.

3. Read `<dir>/report.md`.

Logs print counts only. A new shipped artifact, or a new lockfile, goes
into `shipped-artifacts.toml` in the pull request that adds it. Cutting the
fix release automatically is a documented future goal, not built yet (see
the ADR).

## Docs publishing

`docs-publish.yml` keeps the separate public repository
[`Bookclerk/bookclerk-docs`](https://github.com/Bookclerk/bookclerk-docs)
in step with this one, as Markdown that the website and agents read.

### Main and releases

- **Its `main` mirrors this repository's `main`.** Every push to `main`
  builds the tree and commits it there, with a `Source-Commit: <sha>`
  trailer naming the commit it was built from. A build identical to the
  docs `main` adds no commit.
- **Bursts publish the newest commit only.** Runs on `main` share one
  concurrency group that cancels the older run. A run whose commit is no
  longer the tip of `main` when it reaches the publish step skips it.
- **A published release tags the matching docs commit** (the one whose
  trailer names the release's commit) with the same tag name. When no
  `main` commit matches because a burst skipped it, the release's build is
  committed beside `main`, parented on its head without moving it, and
  that commit is tagged. An existing tag is never moved.

### The tree

The tree is `docs/` as it is, plus `api/`. Only markdownlint's directive
comments (`<!-- markdownlint-disable-line … -->`,
`<!-- markdownlint-disable-next-line … -->`, `<!-- markdownlint-disable … -->`
and the rest) are left out of its pages' prose, since they matter to the
`lint` check alone; code spans and fenced code keep them. The `api/` index lists the
plugin SDKs first (TypeScript, Python, Rust), then the web API, then the
Rust crates and the web UI library.

- **TypeScript**: TypeDoc's Markdown theme for `@bookclerk/plugin-sdk` and
  `ui/src/lib` (`typedoc-plugin-markdown`, a dev dependency of both). One
  page per module, without source links or breadcrumbs.
- **Python**: `scripts/python_api_markdown.py` reads the SDK with `ast` (it
  imports nothing), and renders each public module's `__all__`, with
  Google-style sections as lists and cross-references as links.
- **Rust**: `scripts/rustdoc_markdown.py` renders rustdoc JSON for every
  workspace library from a pinned nightly, one page per public module.
  Items re-exported from private modules are documented where they are
  re-exported. Signatures are rebuilt from the JSON, doc headings sit under
  their item, and intra-doc links come from rustdoc's own link table.
  `api/rust/README.md` groups the crates (plugin interface, plugins, host
  internals, development).
- **Web API**: `scripts/openapi_markdown.py` renders `bookclerkd`'s
  committed OpenAPI document (`crates/bookclerkd/openapi.json`). It writes
  an index with a quick-start request, the credentials and every route, one page per route group (with each
  route's access rule, parameters, body, responses, an example request
  as a `curl` command, a PowerShell `Invoke-RestMethod` call, Python
  (`requests`), JavaScript (`fetch`) and Rust (`reqwest`), and an
  example response for each status, built from the
  schemas or saying why there is no JSON body; the request
  sends only required fields, since optional ones can be alternatives
  that must not be sent together), and a page of
  schemas. Parameters, body fields and responses are lists, one item per
  property (`` `name` (path) *type*, **required** — description ``), with
  an inline object's fields nested under it. A request body that names a
  schema lists that schema's fields in place, and a success response
  whose JSON body is an object lists its fields too, so a route reads
  without a trip to the schemas page; error bodies are shared, so they
  keep the link only.
  - The website lays the route group pages out like an OpenAPI viewer
    (`tools/site/api-reference.ts`): a method badge beside each path, the
    examples in a second column when the reading column is wide enough,
    and the usual "On this page" rail. The rail lists the routes nested
    by path, relative to the start every route of the page shares
    (`/login` on the Auth page, whose headings keep the full path), with
    the shared start of each deeper path written once above the routes
    under it. Every method badge is the same width so the paths line up,
    and the rail is sized to fit the longest path on one line.
  - The property lists become property rows (`tools/site/api-reference.ts`
    `layoutProperties`): name, location, type and a required mark on one
    line, the description under it, a nested object's fields folded with
    their count, and a response's body fields open under its status mark.
    Any other list, and the tables of pages generated before this format,
    pass through unchanged.
  - The example responses become one panel per status with a colored
    status mark, each holding the example body and then, after a
    `Schema:` line, the body's JSON Schema (component references expanded
    in place); the page's script shows the statuses as tabs in one card
    with an Example/Schema switch between the two. The example request's
    samples become one panel per language the same way, and the page's
    script shows them as a language picker. Picking a language or a view
    picks it for every route on the page and is kept in the browser's
    local storage, a display preference the privacy page lists; a page
    restored from the back/forward cache reads the choice again.
  - Like every generated page, each one ends with a Source on GitHub link
    to what it is generated from: `crates/bookclerkd/openapi.json` here.
  - Under **Web API** the sidebar lists every route group and the schemas
    page, not their routes; those are in the rail. The index, titled **Web API** like
    its sidebar entry and breadcrumb, puts its quick start beside the
    introduction and shows each route group as a card of badged routes.
  - The Markdown itself stays plain, so the pages read the same on GitHub.

Every relative link to a file in the tree must resolve, and links in `api/`
must stay inside the tree, or the build fails.

### Last updated dates

`last-updated.json` at the root holds the date (UTC, `YYYY-MM-DD`) each
page of `docs/` last changed, which the website shows as "Last updated".

- It is the author date of the newest commit that changed the page's
  content. A pure move (a rename git sees as unchanged) is followed back to
  the old path rather than counted. Author dates survive history rewrites
  that reset commit dates.
- The workflow checks out the full history and passes
  `--require-history`, so a shallow checkout fails the build. A local build
  from a shallow clone leaves the file out, with a warning.
- The build dates only `docs/`. `docs-publish.py` adds the generated `api/`
  pages, since only it can compare them with the docs repository. A page
  whose content is unchanged there keeps its previous date, and a new or
  changed one gets the publish date.

### Jobs and safety

- **`build`** has no secrets and a read-only token. It runs
  `scripts/docs-markdown.py` and uploads the tree as an artifact.
- **`publish`** checks out only `scripts/docs-publish.py` from the default
  branch. It mints the [Automation App](#automation-app) token for
  `bookclerk-docs` alone (contents: write), and writes one commit through
  the Git Data API, so GitHub signs it as the App.

A manual run publishes only from `main`. The publish script refuses
redirects, so the token is only sent to `api.github.com`.

`docs-markdown.py` clears its output before each build. So it refuses:

- a scratch root (`--scratch-root`, default `target/docs-markdown/`) that
  isn't strictly inside the repository's `target/` or the system temp
  folder;
- an output folder that isn't strictly inside the scratch root;
- an existing output folder that lacks the marker file it writes;
- the filesystem root, the home folder, the repository root, `/mnt` and
  `/tmp`, as either the output or the scratch root.

### Build the tree locally

The tree lands in `target/docs-markdown/out/`:

```sh
rustup toolchain install nightly-2025-11-15 --profile minimal
python3 scripts/docs-markdown.py
```

The nightly is pinned because rustdoc's JSON format changes between
nightlies, and `rustdoc_markdown.py` reads format 56.

- Bump `RUST_NIGHTLY` and `RUST_JSON_FORMAT` in `docs-markdown.py` and the
  workflow together, and adjust the renderer for any change in the format.
- The nightly may predate the workspace's `rust-version`, so the build
  passes `--ignore-rust-version`. It only produces documentation.

## Website checks

`site_audit` (`tools/site/audit/audit.ts`) builds the site, serves it
through the real Worker under `wrangler dev`, and audits it with the
runner's Chrome.

Docs pages are read from this checkout's `docs/` through a local server
(`tools/site/audit/entry.ts`), not from the docs repository. So the audit
covers the docs as the pull request has them, and needs no network. A docs
key the local server fails renders the docs-unavailable page.

| Pages | Lighthouse (mobile and desktop) |
| --- | --- |
| `/`, `/download`, `/docs/guide/getting-started`, `/docs/contributing/ci` | Performance ≥ 90, accessibility 100, best practices ≥ 95, SEO 100, CLS ≤ 0.05 |
| 404 and docs-unavailable | Accessibility 100 |

The 404 and docs-unavailable pages' SEO and best-practice misses, such as
the status code and `noindex`, are intended.

- **Retries.** A run that misses only on performance is repeated once, and
  the better run kept, since lab scores vary on shared runners.
- **Accessibility.** axe-core then checks every page in light and dark
  color schemes, at 1350 and 390 px, against WCAG 2.0 to 2.2 A/AA and its
  best-practice rules. Any violation fails the check.
- **Results.** The results table goes to the job summary. A failed audit
  uploads the Lighthouse reports as the `site-audit` artifact.
- **Chrome starts.** Chrome is started once more when its first start
  never reaches the debugging port (busy runners). The tail of each failed
  start's `chrome-err.log` goes to the job log, and a copy
  (`chrome-err-<attempt>.log`) sits beside the reports when the audit
  fails.

To run it locally:

1. In `tools/site`, run `npm run build`.
2. In `tools/site/audit`, run `npm ci && node audit.ts`. `--chrome <path>`
   or `CHROME_PATH` picks the browser.

## Cloudflare Worker deploys

Three workflows deploy Workers with `wrangler deploy`, sharing the
repository secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`:

| Workflow | Worker | Custom domain |
| --- | --- | --- |
| `site-deploy.yml` | `bookclerk-site` | `<DOMAIN>` |
| `diagnostics-collector-deploy.yml` | `bookclerk-diagnostics` | `diagnostics.<DOMAIN>` |
| `demo-deploy.yml` | `bookclerk-demo` | `demo.<DOMAIN>` |

`DOMAIN` is the organization variable (default `bookclerk.app` for the
site). With `DOMAIN` unset, the diagnostics collector and the demo are
served on the account's workers.dev subdomain instead (see
[Diagnostics](../server/diagnostics.md) and
[`tools/demo/README.md`](../../tools/demo/README.md)).

### The demo deploy

The demo deploy builds the production builder and image from the deployed
commit. Then `wrangler deploy` builds `packaging/demo/Dockerfile` on top of
them and pushes it to Cloudflare's container registry.

It runs when the demo changes on `main`, when a release is published,
nightly, and on demand.

- A nightly or release run first asks the live demo's `/_demo/health`
  which commit it runs. It stops before building when it is the same one
  and an earlier run for that commit finished successfully.
- The Worker reports its version before migrations and smoke checks, so a
  half-finished deploy is retried.
- A repository without the two Cloudflare secrets gets a notice instead of
  a failure.

### API token permissions

The API token is scoped to the Workers by name, so it needs these for each
Worker the workflows deploy:

- **Workers, with the Editor role**, scoped to *Individual Workers* and
  naming the Worker (`bookclerk-site`, `bookclerk-diagnostics`,
  `bookclerk-demo`). This is the current Workers permission. The legacy
  *Workers Scripts: Edit* maps to Editor on all Workers, which is broader
  than the deploys need.
- **Zone → Workers Routes → Edit** (Write in the API reference) on the
  `<DOMAIN>` zone, for the custom domain.

`bookclerk-diagnostics` also needs:

- **Account → Workers KV Storage → Edit**, for the deploy that first
  creates its `TRIAGE_BUDGET` namespace (the binding has no id, so wrangler
  creates it). Deploying bindings needs only Editor on the Worker, so this
  can be removed once the namespace exists.
- **Account → D1 → Edit**. The first deploy creates the dashboard's
  `DASHBOARD_DB` database (no id in the binding), and every deploy applies
  its migrations with `wrangler d1 migrations apply`, so this one stays.

`bookclerk-demo` also needs:

- **Account → D1 → Edit**, for the same reasons (its `DB` database).
- **Account → Containers → Edit**, since every deploy pushes the image and
  updates the container application.
- **Account → Turnstile → Edit**, since every deploy finds or creates the
  `bookclerk-demo` widget, keeps its hostnames current and reads its keys
  (`tools/demo/scripts/worker-secrets.ts`).

### First deploy of a new Worker

Editor can't create a Worker; creating one takes the **Admin** role on the
Workers product. So the first deploy of a new Worker needs one of these:

1. Create the Worker in the Cloudflare dashboard first, under its exact
   name (an empty "Hello World" Worker is fine, because the first deploy
   replaces it). Then add it to the token's Individual Workers scope.
2. Or temporarily give the token the Workers Admin role, run the first
   deploy, then remove the role again and add the new Worker to the
   Individual Workers scope.

See
[Workers roles and permissions](https://developers.cloudflare.com/workers/authorization/workers/).

After that, the two base permissions are enough, plus the per-Worker
permissions that every deploy uses: D1 for `bookclerk-diagnostics` and
`bookclerk-demo`, and Containers and Turnstile for the demo.

> [!WARNING]
> Renaming a Worker in `wrangler.toml` creates a new Worker. That repeats
> this step and leaves the old one behind, so keep the names as they are.

## Next steps

- [Continuous integration](ci.md): the checks these workflows build on.
- [Fuzzing](fuzzing.md): the crash triage that shares the App and the
  bundle keys.
- [Release packaging](packaging.md): the workerd pin bump the App
  proposes.
