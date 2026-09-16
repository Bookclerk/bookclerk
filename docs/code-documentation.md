# Code documentation (Google style)

Bookclerk uses **Google-style** inline API documentation so each language can
generate a browsable reference under [`docs/api/`](api/README.md). Regenerate
with:

```bash
./scripts/generate-api-docs.sh
```

CI runs the same script (docs must build cleanly). Product narrative docs stay
in this `docs/` tree; API reference HTML is generated into `docs/api/{rust,typescript,python,ui}/`
(gitignored — always regenerate locally or in CI artifacts).

## Why

- **crates.io / docs.rs** — guest SDK crates (`bookclerk-plugin-sdk`,
  `bookclerk-plugin-abi`, `bookclerk-plugin-manifest`) render from rustdoc.
- **npm / PyPI** — `@bookclerk/plugin-sdk` (TypeDoc) and
  `bookclerk-plugin-sdk` (pdoc) mirror the same contract.
- **Contributors** — public items are discoverable without reading call sites.

## Universal rules

1. Document every item that Clippy’s documentation lints cover:
   - **Public** items (modules, types, fields, variants, functions, methods,
     constants, traits) via rustc `missing_docs`.
   - **Private** items (including fields of private types and enum-variant
     fields) via Clippy `missing_docs_in_private_items` (workspace lint;
     CI promotes warnings to errors with `RUSTFLAGS=-D warnings`).
2. Start with a **one-sentence summary** in third person (not “This function…”).
3. Follow with details, then language-native sections for arguments, returns,
   errors / throws, and examples when useful.
4. Prefer links to product docs (`docs/plugins.md`, ADRs) over duplicating
   architecture essays in every item.
5. Generated projections (SeaORM `entities/*` field mirrors, ABI `generated.ts` /
   `abi.py` synced from schema) still get field-level summaries — generators
   should emit or preserve comments when possible.
6. Do **not** leave `#![allow(missing_docs)]` on publish surfaces. Narrow
   `#[allow(missing_docs)]` / `#[allow(clippy::missing_docs_in_private_items)]`
   is reserved for vendored / machine-generated sinks that cannot carry comments
   (e.g. protobuf).

### Quality bar (required)

Generated docs must be **human-readable**. Reject identifier-echo stubs:

| Bad (do not ship) | Good |
| --- | --- |
| `/// Cache dir.` | `/// Absolute path to the guest download cache for this fetch.` |
| `/// Message variant.` | `/// Operator-facing error text with no structured code.` |
| `/// Path.` | `/// Filesystem path from host params (jail-granted).` |
| `/** Force. */` | `/** When true, overwrite an existing credential blob. */` |

Minimum expectations:

- **Types / fields / variants:** Explain purpose, units, invariants, and how the
  value is used (not just restating the name). Mention wire names (`camelCase`)
  when they differ from Rust identifiers.
- **Functions / methods:** Include `# Arguments`, `# Returns`, and `# Errors`
  (or JSDoc `@param` / `@returns` / `@throws`, or Google `Args` / `Returns` /
  `Raises`) whenever there is more than a trivial getter. Mechanical CI checks
  **section/tag existence** only (Clippy `missing_errors_doc` /
  `missing_panics_doc` on public Rust items; Oxlint `require-param` /
  `require-returns` on TS API surfaces; Ruff Google `D` on the Python SDK).
  Description *quality* is review-only — lints intentionally do not judge
  whether prose is more than a stub. Publish crates additionally run those
  Clippy section lints on private items via `clippy-publish/clippy.toml`
  (`check-private-items = true`).
- **Modules / crates:** State audience (guest author vs host), feature flags,
  and links to product docs or examples.
- **Examples:** Prefer a short `# Examples` / `@example` on entry-point APIs
  (`BookclerkPlugin`, `parse`, `serve`, CLI helpers). CI compiles every
  rustdoc example (`cargo test --doc`); crates without runnable examples
  report `running 0 tests` and that is not a coverage skip.

## Rust (rustdoc)

Map Google API sections onto rustdoc Markdown:

```rust
/// Brief summary ending with a period.
///
/// Optional longer description of behavior, invariants, and when to call this.
///
/// # Arguments
///
/// * `path` - Absolute path to the guest install directory.
/// * `policy` - Egress policy already approved by the operator.
///
/// # Returns
///
/// Describe metadata including negotiated `api_version`.
///
/// # Errors
///
/// Returns [`SdkError::Protocol`] when the guest speaks an unsupported version.
///
/// # Examples
///
/// ```
/// # use bookclerk_plugin_sdk::BookclerkPlugin;
/// // ...
/// ```
```

| Item | Required |
| --- | --- |
| Crate / module (`//!`) | Purpose, audience (host vs guest), related docs links |
| `pub struct` / `enum` / `trait` | Summary + field/variant docs |
| Private items | One-sentence summary (Clippy `missing_docs_in_private_items`) |
| `pub fn` / methods | Summary; `# Arguments` / `# Returns` / `# Errors` when non-obvious |
| Feature-gated items | Note the feature name (`db`, `tools`, …) |

Workspace lints: `missing_docs = "warn"` and
`clippy::missing_docs_in_private_items = "warn"` (CI promotes warnings to errors
via `RUSTFLAGS="-D warnings"`). Publish-oriented crates also set
`[package.metadata.docs.rs]` so docs.rs builds all features. Publish-crate CI
sets `CLIPPY_CONF_DIR=clippy-publish` (`check-private-items = true`) so
`# Errors` / `# Panics` Clippy denies also cover private items there.

### Mechanical checks (CI)

Structural documentation is enforced automatically; prose usefulness remains a
review responsibility (see GitHub issue #157).

| Surface | Tooling |
| --- | --- |
| Rust `missing_docs` | Workspace lint + `RUSTFLAGS=-D warnings` |
| Rust private docs | Workspace `clippy::missing_docs_in_private_items` + `RUSTFLAGS=-D warnings` |
| Rustdoc HTML / links | `./scripts/generate-api-docs.sh` (deny broken, private, and redundant intra-doc links plus invalid HTML/URLs/codeblocks; publish crates also deny missing crate-level docs) |
| Rust doctests | `cargo test --doc` for every library on the full suite (selected packages when selective CI is on). Most crates have **zero** runnable `# Examples`; that is expected. Behavioral coverage is the `Test` step (`--lib --bins --tests`). |
| Publish-crate Clippy docs | `missing_errors_doc` / `missing_panics_doc` on the ABI/manifest/SDK trio (including private items via `CLIPPY_CONF_DIR=clippy-publish`) |
| Rust public section shape | Same Errors/Panics Clippy denies on selected packages (existence only) |
| TypeScript JSDoc shape | Oxlint: `@param` / `@returns` **existence** on API surfaces (`ui/src/lib/**`, `packages/plugin-sdk/src/**`); description rules off |
| TypeDoc exports | `validation.notDocumented` + `invalidLink` with `treatValidationWarningsAsErrors` |
| Python Google docstrings | Ruff `D` rules with `lint.pydocstyle.convention = "google"` (section shape; prose quality is review-only) |
| UI smoke | `npm run lint` + `npm run test:safe-html` when the UI is affected |
| Private-docs regression | `python3 scripts/tests/test_private_docs_lint.py` (every workspace package has `[lints] workspace = true`) |

Pull requests use a dependency-aware planner ([`scripts/ci-plan.py`](../scripts/ci-plan.py))
so only affected languages/packages run these checks. `merge_group` and `main`
always run the full suite. See [ci.md](ci.md).

`generate-api-docs.sh` selectors (planner emits these; the script does not
re-diff):

```bash
./scripts/generate-api-docs.sh --check --all
./scripts/generate-api-docs.sh --check \
  --rust-package bookclerk-config --typescript-sdk --ui --python
```

## TypeScript / JavaScript (JSDoc → TypeDoc)

Follow the [Google TypeScript style](https://google.github.io/styleguide/tsguide.html)
JSDoc conventions:

```ts
/**
 * Fetches the guest's describe metadata over the host bridge.
 *
 * @param params - Bridge connection options.
 * @returns Metadata including `apiVersion` and plugin kind.
 * @throws {PluginError} When the host rejects the ABI version.
 *
 * @example
 * ```ts
 * const meta = await plugin.describe();
 * ```
 */
export async function describe(params: DescribeOptions): Promise<PluginMetadata> {
```

Export surfaces for TypeDoc: `packages/plugin-sdk` (npm package) and `ui/src`
(operator SPA helpers). React components document props via the props type.

## Python (Google docstrings → pdoc)

Use [Google Python style](https://google.github.io/styleguide/pyguide.html#38-comments-and-docstrings)
docstrings:

```python
def describe(self) -> PluginMetadata:
    """Fetch the guest's describe metadata over the host bridge.

    Returns:
        Metadata including ``api_version`` and plugin kind.

    Raises:
        PluginError: If the host rejects the ABI version.
    """
```

## Shell (Google Shell style)

Every script starts with a header describing purpose, usage, and important
environment variables (see the [Google Shell style guide](https://google.github.io/styleguide/shellguide.html#file-header)):

```bash
#!/usr/bin/env bash
#
# Generate language API references into docs/api/.
#
# Usage:
#   ./scripts/generate-api-docs.sh [--check] [--all]
#   ./scripts/generate-api-docs.sh --rust-package NAME --ui
#
# Environment:
#   CARGO_TARGET_DIR - Optional Cargo target directory (default: target).
#
set -euo pipefail
```
