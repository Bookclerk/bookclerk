# bookclerk-dev

Native Rust implementation of the Bookclerk dev and release packaging workflow,
invoked via [`.cargo/config.toml`](../../.cargo/config.toml) aliases.

## Dev workflow

| Command | What it does |
| --- | --- |
| `cargo build-plugins` | Build all first-party guest binaries |
| `cargo stage-plugins` | Build + stage under `target/plugin-artifacts` |
| `cargo dev-daemon` | Build + stage + run `bookclerkd` with external guests |
| `cargo dev-cli` | Build + stage + run `bookclerk` CLI with external guests |
| `cargo test-staged` | Build + stage + run `staged_plugins` handshake test |

## Release packaging (current OS/arch)

Requires `ui/dist` for host/platform bundles. See [docs/packaging.md](../../docs/packaging.md).

| Command | Output |
| --- | --- |
| `cargo package-plugins` | Per-plugin `.tar.gz`/`.zip` + `SHA256SUMS` → `target/dist/plugins/` |
| `cargo package-hosts` | Host binaries archive → `target/dist/` |
| `cargo package-platform` | Hosts + bundled `sqlite`/`local` plugins → `target/dist/` |

```bash
cd ui && npm ci && npm run build
cargo package-platform
cargo package-plugins
```

CI and GitHub Releases should call the same aliases in an OS matrix, then sign
artifacts (codesign / Authenticode / minisign) — see `docs/packaging.md`.

## Environment

- `BOOKCLERK_FILES_DIR` — default `/tmp/BookclerkFiles`
- `BOOKCLERK_PLUGIN_ARTIFACTS` — staging dir (default `target/plugin-artifacts`)

Implementation: [`src/plugins.rs`](src/plugins.rs), [`src/package.rs`](src/package.rs).
