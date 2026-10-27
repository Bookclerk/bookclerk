# Release packaging

This page explains how Bookclerk is packaged: what ships in a platform
release, how the pinned Cloudflare `workerd` binary is kept current, the
plugin author tools, and how to build the archives yourself.

Platform installs, crates.io plugin installs and CI all use the same
artifact layout (see [Plugin registry](../plugins/registry.md)).

## What pulls from crates.io?

| Artifact | crates.io role | Installed automatically? |
| --- | --- | --- |
| **Host** (`bookclerk`, `bookclerkd`) | Not on crates.io today | No: an OS installer or GitHub Release |
| **Platform plugins** (`sqlite`, `local`) | May publish metadata crates later | **Bundled** in the platform install (`cargo package-platform`) |
| **Storefront plugins** (Audible, Libro, …) | A discovery index, when published | **Opt-in**, through `bookclerk plugins install` (which records a cluster install) |

crates.io is a **catalog**, not a runtime dependency resolver. Nothing is
compiled from crates.io on the operator's machine.

- `bookclerk plugins search` works today.
- `bookclerk plugins install` resolves a **prebuilt HTTPS archive** from
  the package's artifact URLs, verifies it, and records it as an installed
  version. Each host then fetches the archive for its own target, verifies
  its payload digest, and materializes it (see
  [Installing plugins](../server/plugins.md#installing-plugins)).

## What a platform release contains

A platform release ships, so a fresh install works without storefronts
or a Rust toolchain:

- the hosts;
- `bookclerk-jail`, `bookclerk-workerd` and `bookclerk-media-worker`;
- the pinned Cloudflare `workerd` binary;
- the platform plugins (sqlite and local) under
  `plugins/<plugin-key-fs-id>/`, recorded in `install-ledger.json`.

On startup the host installs those bundled plugins from the `plugins/`
folder beside its executable into `$BOOKCLERK_FILES_DIR/plugins/`, and
stamps its own ledger. So any files folder works with a release, including
a container volume (see [Running Bookclerk → Docker](../server/operations.md#docker)).

`cargo build-app --platform` and `cargo ensure-workerd` fetch the pinned
`workerd` into `target/<profile>/`. Reference Echo examples are never
packaged.

### Cloudflare `workerd` pin

The release tag and per-architecture sha256 digests live in
[`crates/bookclerk-workerd/workerd-pin.json`](../../crates/bookclerk-workerd/workerd-pin.json).
The Rust `pin.rs` and the SDK stub copies are generated from it by
`scripts/sync-workerd-pin.py`.

> [!IMPORTANT]
> Don't float "latest" on every `cargo dev`. Hosts and packages must share
> one audited binary.

A daily GitHub Actions job (`.github/workflows/workerd-pin-bump.yml`)
proposes bumps:

1. `scripts/bump-workerd-pin.py` picks the newest Cloudflare `workerd`
   release that is at least **7 days** old. That is the same supply-chain
   cooldown as Dependabot's `cooldown.default-days: 7` in
   `.github/dependabot.yml`. It writes `workerd-pin.json`, then
   regenerates `pin.rs` and the SDK stubs.
2. The job installs `capnproto` (needed to compile `schema/plugin.capnp`),
   rebuilds `bookclerk-workerd`, runs `cargo ensure-workerd`, then runs
   `scripts/test-workerd-echo.sh` against the Echo workerd example guests.
3. It opens `deps/workerd-pin` with the automation App token and turns on
   auto-merge, so the merge queue lands it once its required checks pass
   (see [Automation App](automation.md#automation-app)).

To check by hand, without writing anything:

```sh
python3 scripts/bump-workerd-pin.py --check --cooldown-days 7
python3 scripts/sync-workerd-pin.py --check
```

### Plugin author tools (`check` / `fmt` / `sync-embed` / `package` / `smoke`)

Each language SDK bakes in the same authoring CLI, so there is no
multi-architecture tool binary to ship. Commands and flags match across
languages, and each SDK is **self-contained** for its workerd guest
language (embedding and runtime settings):

| Launcher | Example |
| --- | --- |
| Rust | `cargo plugin -- check .` / `cargo plugin -- smoke .` / `cargo plugin -- package --out dist .` |
| TypeScript | `npx bookclerk-plugin check .` / `npx bookclerk-plugin smoke .` |
| Python | `python -m bookclerk_plugin_sdk check .` / `python -m bookclerk_plugin_sdk smoke .` |

`smoke`:

1. Downloads the pinned Cloudflare `workerd` binary. TypeScript and Python
   don't need the Rust `bookclerk-workerd` launcher.
2. Materializes Cap'n Proto and the bridge, including a per-isolate
   `BRIDGE_TOKEN`.
3. POSTs `describe` (plus the role's `health` route) with
   `Authorization: Bearer …`.

The Rust CLI lives in the **`bookclerk-plugin-tools`** crate (`cargo plugin`
runs its `bookclerk-plugin` binary), and uses the `bookclerk-workerd`
library for `smoke`. Guest plugins depend only on `bookclerk-plugin-sdk`,
so they never pull in workerd download dependencies.
`scripts/sync-workerd-pin.py` keeps the pin and bridge copies in sync
(`--check` in CI).

**Workerd packaging.** Authors import the language package
(`@bookclerk/plugin-sdk/workerd`, `bookclerk_plugin_sdk.workerd`), and
`bookclerk-workerd` injects those modules into the isolate.

- The Python SDK also requires, and writes,
  `compatibility_flags = ["python_workers", "disable_python_external_sdk"]`
  for `.py` mains.
- The optional `sync-embed` vendors SDK sources for offline trees.

The shared contracts are
[`plugin.capnp`](../../crates/bookclerk-plugin-abi/schema/plugin.capnp),
[`plugin-toml.json`](../../crates/bookclerk-plugin-abi/schema/plugin-toml.json),
and the fixtures under
[`crates/bookclerk-plugin-abi/fixtures/tools/`](../../crates/bookclerk-plugin-abi/fixtures/tools/).

## Local packaging (current OS/arch)

Host bundles need `ui/dist`. `cargo package-hosts` rebuilds it when the
SPA sources are stale, or run `cd ui && npm ci && npm run build`.

```sh
cargo package-plugins      # product plugin archives (no examples) → target/dist/plugins/
cargo package-hosts        # hosts + jail + media-worker + bookclerk-workerd + workerd → target/dist/
cargo package-platform     # hosts + helpers + pinned workerd + sqlite + local → target/dist/
```

Archive names follow the [Plugin registry](../plugins/registry.md):

```text
bookclerk-plugin-source-audible-0.1.0-linux-x64-gnu.tar.gz
bookclerk-platform-0.1.0-linux-x64-gnu.tar.gz
```

Each output folder includes `SHA256SUMS*` files for verification.

## Container images

[`.github/workflows/images-publish.yml`](../../.github/workflows/images-publish.yml)
publishes three images to `ghcr.io/<owner>/`, where `<owner>` is the
repository's owner. A fork publishes to its own namespace with its own
`GITHUB_TOKEN`.

| Image | Built from | Architectures |
| --- | --- | --- |
| `bookclerk` | `packaging/docker/Dockerfile` | `amd64`, `arm64` |
| `bookclerk-dev` | `.devcontainer/Dockerfile` | `amd64`, `arm64` |
| `bookclerk-demo` | `packaging/demo/Dockerfile` | `amd64` |

The `bookclerk-dev` image carries inline cache metadata, so the Dev
Container's `build.cacheFrom` reuses its layers. The demo image builds on
the production builder and runtime.

Each architecture builds on a native runner and is checked before it is
pushed, then the architectures are joined into one index:

- The production image boots twice through `packaging/docker/smoke.sh`.
- The Dev Container's tools run as the dev user.

### Tags

- **`sha-<12 hex>`**: built from that `main` commit, and never moved. The
  per-architecture `sha-<12 hex>-<arch>` tags are the index's inputs.
- **`edge`**: the newest build from `main`.
- **`<version>`, `<major>.<minor>`, `<major>`, `latest`**: added when a
  `v<major>.<minor>.<patch>` release is published, by retagging that
  commit's images (built then if missing). Leading zeros and suffixes are
  refused by `scripts/image_release_tags.py`.

### When it runs

The workflow runs:

- nightly when `main` has moved (an image whose `sha-` tag exists is not
  rebuilt);
- on a published release;
- on `main` pushes that change the Dev Container's inputs;
- by hand (a run on any branch but `main` is a dry run).

A pull request that changes the workflow,
[`.github/actions/image-build`](../../.github/actions/image-build/action.yml)
or `scripts/image_release_tags.py` builds and checks every image with a read-only token, and pushes nothing.
Both paths run the same build steps from that action.

### Provenance and permissions

Every published index gets a GitHub build-provenance attestation. It is
signed through Sigstore with the workflow's OIDC identity and stored in the
registry. To check one, replace `<owner>` with the repository's owner:

```sh
gh attestation verify oci://ghcr.io/<owner>/bookclerk:edge --owner <owner>
```

If the attest job fails, re-running the workflow attests the published
index without rebuilding it.

Only the push jobs hold `packages: write`. Only the attest job holds
`id-token: write` and `attestations: write`.

> [!NOTE]
> A package created by the first publish is private. Make each one public
> once in the organization's package settings.

## GitHub Actions (signing sketch — not a contract)

> [!NOTE]
> The workflow below is an **illustrative sketch** for publisher-side
> packaging. Digest pinning, notarization and Authenticode requirements
> are set by follow-up install and trust work, not frozen by this page.

Per-OS matrix builds should run the same cargo aliases, then optionally
sign or notarize:

```yaml
# .github/workflows/release.yml (sketch — subject to change)
jobs:
  package:
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - os: macos-latest
            target: aarch64-apple-darwin
          - os: windows-latest
            target: x86_64-pc-windows-msvc
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
      - uses: actions/setup-node@v7
        with:
          node-version: "26"
      - run: cd ui && npm ci && npm run build
      - run: cargo package-platform --out dist
      - run: cargo package-plugins --out dist/plugins
      # Signing (examples — pick per OS):
      # - Linux: cosign sign-blob / minisign
      # - macOS: codesign + notarytool (after package-platform)
      # - Windows: Authenticode (signtool)
      - uses: actions/upload-artifact@v4
        with:
          name: bookclerk-${{ matrix.target }}
          path: dist/
```

Publish the release assets to GitHub Releases (or S3 or R2), and set
`artifact_base_url` in each plugin crate's `[package.metadata.bookclerk]`
before `cargo publish` on crates.io.

## Next steps

- [`crates/bookclerk-dev/README.md`](../../crates/bookclerk-dev/README.md):
  the development workflow aliases.
- [Plugin registry](../plugins/registry.md): the crates.io taxonomy.
- [Repository automation](automation.md): the jobs that build and bump
  these.
