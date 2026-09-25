# bookclerk-plugin-sdk

Guest-only Bookclerk plugin SDK for Rust authors of native and workerd plugins.

Depend on this crate from a standalone plugin workspace — not
`bookclerk-plugin-host`. Implement [`PluginWorker`] and call [`serve`] (native Cap'n Proto) or use the workerd /
npm bridge described in `src/workerd.rs`.

## Features

| Feature | Purpose |
| --- | --- |
| *(default)* | Cap'n Proto guest runner, fetch/upload path helpers, callback tunnel, ABI re-exports |
| `db` | SeaORM ↔ Workers RPC DTO helpers for database guests |
| `http` | Native HTTPS through the workerd socket proxy |

The `bookclerk-plugin` author CLI (`check` / `fmt` / `sync-embed` / `package` /
`smoke`) lives in [`bookclerk-plugin-tools`](../bookclerk-plugin-tools), so
guests depending on this SDK never pull `bookclerk-workerd`:

```bash
cargo plugin -- check .
# same as: cargo run -p bookclerk-plugin-tools --bin bookclerk-plugin -- check .
```

## API documentation

Rustdoc for this crate is generated with the workspace API docs:

```bash
./scripts/generate-api-docs.sh
# or
cargo doc -p bookclerk-plugin-sdk --no-deps --all-features --open
```

HTML lands under `docs/api/rust/` (gitignored). Style expectations are in
[`docs/code-documentation.md`](../../docs/code-documentation.md). Product
guides: [`docs/plugins.md`](../../docs/plugins.md),
[`docs/plugin-registry.md`](../../docs/plugin-registry.md).
