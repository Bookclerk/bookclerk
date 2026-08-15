# Third-party plugin publisher workflow

Bookclerk ships a **reusable GitHub Actions workflow** that third-party plugin
authors can copy or call. Bookclerk’s own CI **does not** build your repository.

Reusable workflow in this monorepo:

```text
.github/workflows/plugin-publisher-reusable.yml
```

(A dedicated `bookclerk-plugin-workflows` repository may host the same file
later; until then, copy it or reference this path.)

## What it does

Per matrix runner (`ubuntu-latest` → `linux-x64-gnu`, `macos-latest` →
`macos-arm64`, `windows-latest` → `windows-x64`):

1. `cargo build --release -p <plugin_package>`
2. Stages `plugin.toml` + the release binary
3. When the repo depends on `bookclerk-plugin-sdk`, runs
   `bookclerk-plugin check` + `package` (same tools as local authoring).
   Otherwise packs with `tar` into
   `{crate}-{version}-{bookclerk_target}.tar.gz` (Unix) or `.zip` (Windows)
4. Writes `SHA256SUMS` next to the archive
5. Optionally runs [`actions/attest@v4`](https://github.com/actions/attest)
   when `attest: true`
6. Uploads the archive + checksums as workflow artifacts

Node / Python publishers can call the same commands via
`npx bookclerk-plugin` or `python -m bookclerk_plugin_sdk` in their own
workflows (see [packaging.md](../../docs/packaging.md#plugin-author-tools-check--fmt--package)).

## Caller permissions (attestations)

When you set `attest: true`, **both** the caller workflow and the reusable
workflow need:

```yaml
permissions:
  contents: read
  id-token: write      # mint OIDC token for Sigstore
  attestations: write  # persist the attestation on the repo
```

Omit attestation (default) if you only need archives + SHA-256 sums.

## Example caller workflow

Place this in **your** plugin repo (not Bookclerk). Adjust package / id / kind
and the path to `plugin.toml`.

```yaml
# .github/workflows/release-plugin.yml
name: Release plugin

on:
  push:
    tags: ["v*"]
  workflow_dispatch:

permissions:
  contents: read
  id-token: write
  attestations: write

jobs:
  package:
    # Copy plugin-publisher-reusable.yml into this repo, or reference Bookclerk:
    uses: fritz-fritz/bookclerk/.github/workflows/plugin-publisher-reusable.yml@main
    # uses: ./.github/workflows/plugin-publisher-reusable.yml
    with:
      plugin_package: bookclerk-plugin-integration-echo
      plugin_id: echo
      kind: integration
      plugin_toml: plugin.toml
      # version: ""   # default: Cargo.toml package version
      attest: true
```

After the job finishes, download the matrix artifacts, attach them to a GitHub
Release (or upload to S3/R2/CDN), and pin digests in your static registry /
`[package.metadata.bookclerk].artifacts[]` entries. See
[docs/plugin-registry.md](../../docs/plugin-registry.md).

## Layout assumptions

| Input | Expectation |
| --- | --- |
| `plugin_package` | Cargo package name **and** binary name under `target/release/` |
| `plugin_toml` | Path relative to repo root (default `plugin.toml`) |
| Archive contents | `plugin.toml` + executable (`.exe` on Windows) at archive root |

## Related experimental templates

- TypeScript (workerd, id `echo_native_node`): [`../plugins-echo-native-node/`](../plugins-echo-native-node/)
- Python (workerd, id `echo_native_python`): [`../plugins-echo-native-python/`](../plugins-echo-native-python/)
- Workerd TypeScript: [`../plugins-echo-workerd-ts/`](../plugins-echo-workerd-ts/)
- Native Rust: [`../plugins-echo-native-rust/`](../plugins-echo-native-rust/)
