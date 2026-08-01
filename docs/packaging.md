# Release packaging

Bookclerk uses the same artifact layout for **platform installs**, **crates.io
plugin install**, and **CI** — see [plugin-registry.md](plugin-registry.md).

## What pulls from crates.io?

| Artifact | crates.io role | Auto-installed? |
| --- | --- | --- |
| **Host** (`bookclerk`, `bookclerkd`) | Not on crates.io today | No — OS installer / GitHub Release |
| **Platform plugins** (`sqlite`, `local`) | May publish metadata crates later | **Bundled** in platform install (`cargo package-platform`) |
| **Storefront plugins** (Audible, Libro, …) | Discovery index when published | **Opt-in** via `bookclerk plugins install` (Phase C) |

crates.io is a **catalog**, not a runtime dependency resolver. Nothing is compiled
from crates.io on the operator machine. `bookclerk plugins search` works today;
`bookclerk plugins install` downloads a **prebuilt HTTPS archive** using
`[package.metadata.bookclerk].artifact_*` URLs (not yet implemented).

Platform releases ship hosts + `plugins/sqlite/` + `plugins/local/` so a fresh
install works without storefronts or a Rust toolchain.

## Local packaging (current OS/arch)

Requires `ui/dist` for host bundles (`cd ui && npm ci && npm run build`).

```bash
cargo package-plugins      # per-plugin archives → target/dist/plugins/
cargo package-hosts        # host binaries only → target/dist/
cargo package-platform     # hosts + sqlite + local → target/dist/
```

Archive names follow [plugin-registry.md](plugin-registry.md):

```text
bookclerk-plugin-source-audible-0.1.0-x86_64-unknown-linux-gnu.tar.gz
bookclerk-platform-0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

Each output directory includes `SHA256SUMS*` files for verification.

## GitHub Actions (signing sketch — not a contract)

The workflow below is an **illustrative sketch** for publisher-side packaging.
Digest pinning, notarization, and Authenticode requirements are defined in
follow-up install/trust work — not frozen by this document.

Per-OS matrix builds should run the same cargo aliases, then optionally
sign/notarize:

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
          node-version: "22"
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

Publish release assets to GitHub Releases (or S3/R2) and set
`artifact_base_url` in each plugin crate's `[package.metadata.bookclerk]` before
`cargo publish` on crates.io.

## Related

- Dev workflow aliases: [`crates/bookclerk-dev/README.md`](../crates/bookclerk-dev/README.md)
- crates.io taxonomy: [plugin-registry.md](plugin-registry.md)
