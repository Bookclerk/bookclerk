# bookclerk-dev

Native Rust implementation of the Bookclerk dev and release packaging workflow,
invoked via [`.cargo/config.toml`](../../.cargo/config.toml) aliases.

## Taxonomy

| Tier | What | Where |
| --- | --- | --- |
| **Platform** | Hosts + jail/workerd/media-worker + `sqlite` + `local` | `default-members` + `crates/bookclerk-plugins/platform/*` → install into `$BOOKCLERK_FILES_DIR/plugins/` |
| **Optional** | Storefronts, ABS, s3, d1, postgres | `crates/bookclerk-plugins/optional/*` → `cargo stage-plugins --optional` |
| **Examples** | Echo under `examples/` | `cargo stage-plugins --examples` — **never packaged** |

Every guest is an **external** (jailed) subprocess. Runtimes are **`native`** or **`workerd`**.

## Dev workflow

| Command | What it does |
| --- | --- |
| `cargo build-app --platform` | Full installer: all `default-members` + platform guests + ensure pinned `workerd` |
| `cargo build-app --optional` | Optional guest crates |
| `cargo build-app --examples` | Cargo-backed examples |
| `cargo build-app --print …` | Print resolved `-p` names (no duplicates) |
| `cargo ensure-workerd` | Download/update pinned Cloudflare `workerd` → `target/<profile>/` |
| `cargo install-platform` | Install platform guests → `$BOOKCLERK_FILES_DIR/plugins/` |
| `cargo stage-plugins --optional` | Stage optional guests → `target/plugin-artifacts` |
| **`cargo dev`** | Platform build + refresh `ui/dist` if stale + ensure workerd, install, exec `bookclerkd` |
| `cargo dev --optional` | Also build/stage optional storefronts |
| `cargo dev --examples` | Also stage reference Echo |
| `cargo dev-cli` | Same platform build, then CLI binary |
| `cargo test-staged` | Full platform + optional + examples + conformance |
| **`cargo reset --yes`** | Wipe `$BOOKCLERK_FILES_DIR` (DB, config, keys, plugins) |
| `cargo reset --yes --artifacts` | Also clear `target/plugin-artifacts` |

Default `cargo dev` is **one** `cargo build` for the platform package list (not a
second wave for helpers/hosts), plus a Vite rebuild of `ui/dist` when SPA sources
are newer than `ui/dist/index.html`. Optional guests are **not** on that list until
you pass `--optional`. CI uses
`build-app --platform --optional --examples` for the full graph.

Dev data defaults to `<workspace>/BookclerkFiles` (gitignored). After a schema
migration mismatch (`DatabaseTooFarAhead`), run `cargo reset --yes` then
`cargo dev` again.

Adding a guest = new directory under `platform/` or `optional/` with `Cargo.toml`
+ `plugin.toml` (workspace globs pick it up).

```bash
cargo build-app --platform --print
cargo reset --yes          # clean app data when the local DB is stale
cargo dev
cargo dev --optional --examples
```

## Release packaging

| Command | Output |
| --- | --- |
| `cargo package-plugins` | **Optional** guest archives → `target/dist/plugins/` |
| `cargo package-hosts` | Hosts + helpers → `target/dist/` |
| `cargo package-platform` | Hosts + helpers + sqlite + local → `target/dist/` |

Implementation: [`src/plugins.rs`](src/plugins.rs) (directory discovery), [`src/package.rs`](src/package.rs).
