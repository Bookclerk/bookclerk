# bookclerk-plugin-sdk

Guest-only Bookclerk plugin SDK for Rust authors of native and workerd plugins.

Depend on this crate from a standalone plugin workspace — not
`bookclerk-plugin-host`. Implement [`BookclerkPlugin`] and serve it with
[`BookclerkPluginGuest`] (native stdio) or the workerd / npm bridge described in
`src/workerd.rs`.

## Features

| Feature | Purpose |
| --- | --- |
| *(default)* | Stdio guest runner, FD side channels, callback tunnel, ABI re-exports |
| `db` | SeaORM ↔ Workers RPC DTO helpers for database guests |
| `tools` | `bookclerk-plugin` author CLI (`check` / `fmt` / `package` / `smoke`) |

Guest plugins should leave `tools` off so they do not pull `bookclerk-workerd`.

```bash
cargo run -p bookclerk-plugin-sdk --features tools --bin bookclerk-plugin -- check .
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

## See also

- `bookclerk-plugin-abi` — wire DTOs and method names
- `bookclerk-plugin-manifest` — `plugin.toml` parse / format
- Examples under `examples/plugins-echo-*`
