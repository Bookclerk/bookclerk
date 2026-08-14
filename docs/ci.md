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
| `fmt / clippy / test` | Selective steps driven by plan outputs (when `SELECTIVE_CI=1`) |
| `release build` | When hosts/platform packaging are affected (or full suite) |
| `sandbox + jailed tiers` | When confinement packages are affected (or full suite) |
| `tray` | When `bookclerk-tray` is affected (or full suite) |
| `CI Gate` | Stable required check: succeeds for intentional skips; fails on real failures |

OSV scanning remains a separate workflow/gate.

## Branch protection / merge queue

Configure repository rulesets / branch protection so the **required** CI check
is **`CI Gate`** (plus the OSV check), **not** every matrix child. Skipped
`release` / `confinement` / `tray` jobs report `skipped`; if those job names are
individually required, merges stay pending forever.

Also enable a merge queue that consumes `merge_group` checks so the full suite
runs on the synthetic merge commit before landing.

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
