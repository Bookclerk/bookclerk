# Continuous integration

Bookclerk’s GitHub Actions workflow (`.github/workflows/ci.yml`) uses a
**dependency-aware planner** so pull requests can skip unrelated work, while
`merge_group` and pushes to `main` always run the full suite.

## Shadow → selective

`SELECTIVE_CI` in `.github/workflows/ci.yml` is **`0`** (shadow): the planner
still publishes predictions (`full_suite` and surface flags), but
`execute_full_suite` stays true so every command branch runs the full baseline.
Flip to `1` in a small follow-up after representative docs-only, UI-only,
binary-only, leaf-crate, and shared-crate PRs have executed the selective
paths. `merge_group` / `main` always `--force-full`.

## Planner

```bash
python3 scripts/ci-plan.py --base <sha> --head <sha> --format summary
python3 scripts/tests/test_ci_plan.py -q
```

The planner:

1. Diffs changed paths (`git diff --name-only base...head`).
2. Maps every file under a Cargo package root to that package (not only `.rs`).
3. Loads workspace members and path-dependency edges from
   `cargo metadata --no-deps`.
4. Expands each changed package to its reverse-transitive dependents.
5. Classifies non-Cargo surfaces (`ui/`, `packages/plugin-sdk/`,
   `packages/plugin-sdk-python/`, docs, plugin tiers).
6. **Fails closed** for any changed path that is neither a Cargo package member
   nor an explicit non-Cargo classifier (e.g. `third_party/**`,
   `.github/actions/**`, arbitrary `scripts/**`, unknown `packages/<name>/**`).
7. Emits JSON / GitHub Actions outputs and a step summary explaining every
   run/skip decision. `rust_doc_packages` feeds `cargo doc`;
   `rust_doctest_packages` is the lib+`doctest` subset used for
   `cargo test --doc`. Binary-only crates are excluded from
   `rust_doctest_packages`, but remain in `rust_doc_packages` and are rendered
   by `cargo doc` (Cargo’s `--doc` test target is library-only, whereas
   `cargo doc` documents selected binary and library targets and includes
   private items for binaries by default). Workspace Clippy also enforces
   `missing_docs_in_private_items` (see `docs/code-documentation.md`).

Conservative **full suite** triggers include root `Cargo.toml` / `Cargo.lock`,
`rust-toolchain.toml`, `.cargo/**`, CI workflows, the planner itself, unresolved
package manifests, unknown top-level paths, unclassified paths under known roots,
and planner failures. There is **no** per-crate lane metadata — specialization
roots are discovered from directories (confinement, tray, platform/optional/examples
plugins, SDKs).

## Jobs

| Job | Role |
| --- | --- |
| `plan` | Always runs; publishes outputs + `ci-plan` artifact |
| `fmt / clippy / test` | Selective steps driven by plan outputs (when `SELECTIVE_CI=1`). Installs `capnproto`. The plugin ABI contract requires pinned `target/debug/workerd` (fails closed unless a local `BOOKCLERK_SKIP_WORKERD=1` skip is used — CI never sets that) |
| `release build` | When hosts/platform packaging are affected (or full suite). Installs `capnproto`. |
| `sandbox + jailed tiers` | When confinement packages are affected (or full suite). Windows runs `--test-threads=1` so parallel AppContainer tests do not starve `Local\bookclerk-dacl-tx`. Windows also installs Cap'n Proto and clippy/tests `bookclerk-workerd` (named-pipe SOCKET_PROXY), `bookclerk-plugin-sdk` with `http`, and `bookclerk-plugin-host --lib` so `#[cfg(windows)]` nested Deny is compiled. |
| `tray` | When `bookclerk-tray` is affected (or full suite) |
| `postgres 16/17/18` | When Rust runs (or full suite). Matrix of every supported PostgreSQL major ≥ 16 (`fail-fast: false`; `CI Gate` requires the whole job). Installs `capnproto`. Requires a Postgres service at that major. Runs ignored job-queue tests, TOTP atomic conformance, shared SQL-plan vectors, guest page tests, binding-schema isolation, and production RPC LIKE. |
| `CI Gate` | Stable required check: succeeds for intentional skips; fails on real failures |

OSV scanning remains a separate workflow/gate.

The scheduled **SQL-v1 property** job (`.github/workflows/sql-v1-proptest.yml`)
runs a longer `proptest` suite (`PROPTEST_CASES=256`) on `bookclerk-plugin-abi`
and `bookclerk-db-exec`, grammar-aware sqlite admission plus a PostgreSQL 16
differential (normalized `DbValue` / `DbErrorClass`), and coverage-guided
`cargo-fuzz` targets (`sql_parse`, `sql_lower`) with checked-in corpora under
`fuzz/corpus/`. It is `workflow_dispatch` + weekly; PR CI keeps the modest
in-crate case counts, deterministic corpus replay, and the postgres-jobs
matrix (binding UTF8 readiness + sqlite/postgres differential).

The scheduled **workerd pin bump** job (`.github/workflows/workerd-pin-bump.yml`)
also compiles `bookclerk-workerd` / `bookclerk-plugin-abi`, so it installs
`capnproto` before `cargo build` when a pin bump is eligible.

## Branch protection / merge queue

Configure repository rulesets / branch protection so the **required** CI check
is **`CI Gate`** (plus the OSV check), **not** every matrix child. Skipped
`release` / `confinement` / `tray` jobs report `skipped`; if those job names are
individually required, merges stay pending forever.

Also enable a merge queue that consumes `merge_group` checks so the full suite
runs on the synthetic merge commit before landing. Merge queues are only
available on **organization-owned** repositories (free for public repos), so
this step waits until the repository lives in an organization. Queue settings:
squash, "only merge non-failing pull requests", build concurrency around 5.
With a queue required, "require branches to be up to date" no longer matters;
the queue tests each entry against current `main` stacked on the entries ahead
of it.

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

## Expected PR feedback (when `SELECTIVE_CI=1`)

For a **documentation-only** change (`docs/**` only):

- `plan` runs (planner unit tests + path plan)
- `fmt / clippy / test` runs **format only** (no clippy/test/docs generation/build-app)
- `release` / `confinement` / `tray` are **skipped**
- `CI Gate` succeeds

Wall time target: under **three minutes** excluding Actions queueing (planner
itself is sub-second; the remaining cost is checkout + rustfmt toolchain).

For `merge_group` / `main`, the planner forces `full_suite=true` so every job
and the strengthened documentation checks still run.
