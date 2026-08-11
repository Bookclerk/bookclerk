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

1. Document every **public** item (modules, types, fields, variants, functions,
   methods, constants, traits). Private helpers need docs only when non-obvious.
2. Start with a **one-sentence summary** in third person (not “This function…”).
3. Follow with details, then language-native sections for arguments, returns,
   errors / throws, and examples when useful.
4. Prefer links to product docs (`docs/plugins.md`, ADRs) over duplicating
   architecture essays in every item.
5. Generated projections (SeaORM `entities/*` field mirrors, ABI `generated.ts` /
   `abi.py` synced from schema) still get field-level summaries — generators
   should emit or preserve comments when possible.
6. Do **not** leave `#![allow(missing_docs)]` on publish surfaces. Narrow
   `#[allow(missing_docs)]` is reserved for vendored / machine-generated sinks
   that cannot carry comments (e.g. protobuf).

### Quality bar (required)

Generated docs must be **human-readable**. Reject identifier-echo stubs:

| Bad (do not ship) | Good |
| --- | --- |
| `/// Cache dir.` | `/// Absolute path to the guest download cache for this fetch.` |
| `/// Message variant.` | `/// Operator-facing error text with no structured code.` |
| `/// Path.` | `/// Filesystem path resolved from the host side-channel FD or params.` |
| `/** Force. */` | `/** When true, overwrite an existing credential blob. */` |

Minimum expectations:

- **Types / fields / variants:** Explain purpose, units, invariants, and how the
  value is used (not just restating the name). Mention wire names (`camelCase`)
  when they differ from Rust identifiers.
- **Functions / methods:** Include `# Arguments`, `# Returns`, and `# Errors`
  (or JSDoc `@param` / `@returns` / `@throws`, or Google `Args` / `Returns` /
  `Raises`) whenever there is more than a trivial getter.
- **Modules / crates:** State audience (guest author vs host), feature flags,
  and links to product docs or examples.
- **Examples:** Prefer a short `# Examples` / `@example` on entry-point APIs
  (`BookclerkPlugin`, `parse`, `serve`, CLI helpers).

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
/// Handshake result including negotiated `api_version`.
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
| `pub fn` / methods | Summary; `# Arguments` / `# Returns` / `# Errors` when non-obvious |
| Feature-gated items | Note the feature name (`db`, `tools`, …) |

Workspace lint: `missing_docs = "warn"` (CI treats warnings as errors via
`RUSTFLAGS=-D warnings`). Publish-oriented crates also set
`[package.metadata.docs.rs]` so docs.rs builds all features.

## TypeScript / JavaScript (JSDoc → TypeDoc)

Follow the [Google TypeScript style](https://google.github.io/styleguide/tsguide.html)
JSDoc conventions:

```ts
/**
 * Runs the guest handshake against the host bridge.
 *
 * @param params - Negotiated install identity and capabilities.
 * @returns Result including `apiVersion` and plugin kind.
 * @throws {PluginError} When the host rejects the ABI version.
 *
 * @example
 * ```ts
 * const result = await plugin.handshake({ apiVersion: 1 });
 * ```
 */
export async function handshake(params: HandshakeParams): Promise<HandshakeResult> {
```

Export surfaces for TypeDoc: `packages/plugin-sdk` (npm package) and `ui/src`
(operator SPA helpers). React components document props via the props type.

## Python (Google docstrings → pdoc)

Use [Google Python style](https://google.github.io/styleguide/pyguide.html#38-comments-and-docstrings)
docstrings:

```python
def handshake(self, params: HandshakeParams) -> HandshakeResult:
    """Run the guest handshake against the host bridge.

    Args:
        params: Negotiated install identity and capabilities.

    Returns:
        Result including ``api_version`` and plugin kind.

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
#   ./scripts/generate-api-docs.sh [--check]
#
# Environment:
#   CARGO_TARGET_DIR - Optional Cargo target directory (default: target).
#
set -euo pipefail
```

Non-obvious blocks get a short `#` comment above them. There is no separate HTML
generator for shell; headers are the documentation.

## Publishing checklist (guest SDKs)

Before flipping `publish = true` / npm publish / PyPI upload:

1. `./scripts/generate-api-docs.sh` succeeds.
2. Crate/package README links to the generated reference and product docs.
3. Rust: `[package.metadata.docs.rs]` features match what authors need.
4. No `allow(missing_docs)` on the published crate root.
5. Examples in docs compile (`cargo test --doc -p bookclerk-plugin-sdk`).
