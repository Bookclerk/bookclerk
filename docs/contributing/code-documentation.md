# Code documentation (Google style)

This page explains how to document code in Bookclerk: the rules every
language follows, the quality bar, the per-language formats, and the
checks CI runs.

Bookclerk uses **Google-style** inline API documentation, so each language
can generate a browsable reference under [`docs/api/`](../api/README.md).
Regenerate it with:

```sh
./scripts/generate-api-docs.sh
```

CI runs the same script, and the docs must build cleanly.

- Product docs stay in this `docs/` tree.
- API reference HTML is generated into
  `docs/api/{rust,typescript,python,ui}/`. It is gitignored, and CI
  doesn't keep it, so regenerate it locally.

## Why

- **crates.io and docs.rs:** the guest SDK crates
  (`bookclerk-plugin-sdk`, `bookclerk-plugin-abi`,
  `bookclerk-plugin-manifest`) render from rustdoc.
- **npm and PyPI:** `@bookclerk/plugin-sdk` (TypeDoc) and
  `bookclerk-plugin-sdk` (pdoc) mirror the same contract.
- **Contributors:** public items can be found without reading call sites.

## Universal rules

1. Document every item that Clippy's documentation lints cover:
   - **Public** items (modules, types, fields, variants, functions,
     methods, constants, traits), through rustc `missing_docs`.
   - **Private** items (including fields of private types and enum-variant
     fields), through Clippy `missing_docs_in_private_items`. It is a
     workspace lint, and CI promotes warnings to errors with
     `RUSTFLAGS=-D warnings`.
2. Start with a **one-sentence summary** in the third person (not "This
   function…").
3. Follow it with details, then the language's own sections for
   arguments, returns, errors or throws, and examples when useful.
4. Prefer links to product docs (`docs/plugins/README.md`, ADRs) over
   repeating architecture essays in every item.
5. Generated projections (SeaORM `entities/*` field mirrors, the ABI's
   `generated.ts` and `abi.py` synced from the schema) still get
   field-level summaries. Generators should emit or keep comments when
   they can.
6. Don't leave `#![allow(missing_docs)]` on publish surfaces. A narrow
   `#[allow(missing_docs)]` or
   `#[allow(clippy::missing_docs_in_private_items)]` is only for vendored
   or machine-generated code that can't carry comments (such as protobuf).

### Quality bar (required)

Generated docs must be **readable by people**. Reject stubs that echo the
identifier:

| Bad (don't ship) | Good |
| --- | --- |
| `/// Cache dir.` | `/// Absolute path to the guest download cache for this fetch.` |
| `/// Message variant.` | `/// Operator-facing error text with no structured code.` |
| `/// Path.` | `/// Filesystem path from host params (jail-granted).` |
| `/** Force. */` | `/** When true, overwrite an existing credential blob. */` |

The minimum for each kind of item:

- **Types, fields and variants.** Explain the purpose, units and
  invariants, and how the value is used, not just the name again. Mention
  wire names (`camelCase`) when they differ from the Rust identifiers.
- **Functions and methods.** Include `# Arguments`, `# Returns` and
  `# Errors` (or JSDoc `@param`, `@returns`, `@throws`, or Google `Args`,
  `Returns`, `Raises`) for anything more than a trivial getter.
- **Modules and crates.** State the audience (guest author or host), the
  feature flags, and links to product docs or examples.
- **Examples.** Prefer a short `# Examples` or `@example` on entry-point
  APIs (`PluginWorker`, `parse`, `serve`, CLI helpers). CI compiles every
  rustdoc example (`cargo test --doc`). Crates without runnable examples
  report `running 0 tests`, and that isn't a skipped check.

> [!NOTE]
> CI checks only that **sections and tags exist**: Clippy
> `missing_errors_doc` and `missing_panics_doc` on public Rust items,
> Oxlint `require-param` and `require-returns` on TypeScript API surfaces,
> and Ruff Google `D` rules on the Python SDK. Whether the description is
> more than a stub is for review. Publish crates also run those Clippy
> section lints on private items, through `clippy-publish/clippy.toml`
> (`check-private-items = true`).

## Rust (rustdoc)

Map the Google API sections onto rustdoc Markdown:

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
/// # use bookclerk_plugin_sdk::PluginWorker;
/// // ...
/// ```
```

| Item | Required |
| --- | --- |
| Crate or module (`//!`) | Purpose, audience (host or guest), links to related docs |
| `pub struct` / `enum` / `trait` | A summary, plus field and variant docs |
| Private items | A one-sentence summary (Clippy `missing_docs_in_private_items`) |
| `pub fn` and methods | A summary; `# Arguments` / `# Returns` / `# Errors` when not obvious |
| Feature-gated items | The feature name (`db`, `tools`, …) |

The workspace lints are `missing_docs = "warn"` and
`clippy::missing_docs_in_private_items = "warn"`, and CI promotes warnings
to errors with `RUSTFLAGS="-D warnings"`.

- Publish-oriented crates also set `[package.metadata.docs.rs]`, so
  docs.rs builds all features.
- Publish-crate CI sets `CLIPPY_CONF_DIR=clippy-publish`
  (`check-private-items = true`), so the `# Errors` and `# Panics` Clippy
  denies cover private items there too.

### Mechanical checks (CI)

Structural documentation is enforced automatically. Whether the prose is
useful stays a review responsibility (see GitHub issue #157).

| Surface | Tooling |
| --- | --- |
| Rust `missing_docs` | Workspace lint + `RUSTFLAGS=-D warnings` |
| Rust private docs | Workspace `clippy::missing_docs_in_private_items` + `RUSTFLAGS=-D warnings` |
| Rustdoc HTML and links | `./scripts/generate-api-docs.sh` (see below) |
| Rust doctests | `cargo test --doc` (see below) |
| Publish-crate Clippy docs | `missing_errors_doc` / `missing_panics_doc` on the ABI, manifest and SDK crates, including private items (`CLIPPY_CONF_DIR=clippy-publish`) |
| Rust public section shape | The same Errors/Panics Clippy denies on selected packages (existence only) |
| TypeScript JSDoc shape | Oxlint: `@param` / `@returns` **existence** on API surfaces (`ui/src/lib/**`, `packages/plugin-sdk/src/**`); description rules off |
| TypeDoc exports | `validation.notDocumented` + `invalidLink` with `treatValidationWarningsAsErrors` |
| Python Google docstrings | Ruff `D` rules with `lint.pydocstyle.convention = "google"` (section shape only) |
| UI smoke | `npm run lint` + `npm run test:safe-html` when the UI is affected |
| CLI reference | `cli_reference_is_current` in `bookclerk-cli` (see below) |
| Web API reference | `the_committed_document_is_current` in `bookclerkd` (see below) |
| Private-docs regression | `python3 scripts/tests/test_private_docs_lint.py` (every workspace package has `[lints] workspace = true`) |

Some of these need more than a cell:

- **Rustdoc HTML and links.** The script denies broken, private and
  redundant intra-doc links, plus invalid HTML, URLs and code blocks.
  Publish crates also deny missing crate-level docs.
- **Rust doctests.** `cargo test --doc` runs for every library on the full
  suite (selected packages when selective CI is on). Most crates have
  **zero** runnable `# Examples`, and that is expected. Behavioral coverage
  is the `Test` step (`--tests`: library and binary unit tests, plus
  integration tests).
- **CLI reference.** The test fails when
  [the CLI reference](../server/cli-reference.md) no longer matches the
  clap definitions. Regenerate it with
  `UPDATE_CLI_REFERENCE=1 cargo test -p bookclerk-cli cli_reference`.
- **Web API reference.** The test fails when
  `crates/bookclerkd/openapi.json` no longer matches the routes.
  Regenerate it with
  `cargo run -p bookclerkd -- --print-openapi > crates/bookclerkd/openapi.json`.

Pull requests use a dependency-aware planner
([`scripts/ci-plan.py`](../../scripts/ci-plan.py)), so only the affected
languages and packages run these checks. `merge_group` always runs the
full suite. Pushes to `main` only build and save caches, because the merge
group already tested the commit. See
[Continuous integration](ci.md#build-only-main-pushes).

The planner emits these `generate-api-docs.sh` selectors; the script
doesn't diff again itself:

```sh
./scripts/generate-api-docs.sh --check --all
./scripts/generate-api-docs.sh --check \
  --rust-package bookclerk-config --typescript-sdk --ui --python
```

## TypeScript and JavaScript (JSDoc → TypeDoc)

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

The export surfaces for TypeDoc are `packages/plugin-sdk` (the npm
package) and `ui/src` (the operator SPA helpers). React components
document their props through the props type.

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

Every script starts with a header describing its purpose, usage and
important environment variables (see the
[Google Shell style guide](https://google.github.io/styleguide/shellguide.html#file-header)):

```sh
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

## Next steps

- [Writing docs](writing-docs.md): how to write the product docs pages.
- [API reference](../api/README.md): the generated reference.
- [Continuous integration](ci.md): where these checks run.
