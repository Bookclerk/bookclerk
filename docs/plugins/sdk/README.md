# Plugin SDKs

This folder holds one guide per SDK language, each followed by the
reference generated from that SDK's code. Pick the language you write
in; the plugin model is the same in all three, and
[Dynamic plugins](../README.md) explains it.

## Choose an SDK

| Language | Package | Runs as | Guide |
| --- | --- | --- | --- |
| TypeScript | `@bookclerk/plugin-sdk` | A workerd isolate | [TypeScript plugin SDK](typescript/README.md) |
| Python | `bookclerk-plugin-sdk` | A workerd isolate (Python Workers) | [Python plugin SDK](python/README.md) |
| Rust | `bookclerk-plugin-sdk` | A native binary, or a workerd isolate through Wasm | [Rust plugin SDK](rust/README.md) |

Third-party plugins depend on the SDK only, never on
`bookclerk-plugin-host`, `bookclerk-library` or `bookclerk-source`.
First-party native storefront guests add their private store crates and
`bookclerk-storefront`, the slim crate of storefront types they share with
the host (the Audible guest also links `bookclerk-audible-catalog` and
`bookclerk-mp4`, and the Libro.fm, Chirp and GraphicAudio guests link
`bookclerk-scan-delta` for their scan deltas). It has no database, ORM, configuration or OpenAPI dependencies,
so a guest build stays small.

## Runtimes

`plugin.toml` names one of two runtimes:

| `runtime` | How it runs |
| --- | --- |
| `workerd` | Author `modules/` loaded by [`bookclerk-workerd`](../../../crates/bookclerk-workerd): one jail and one isolate per plugin (a local embed, not Cloudflare cloud execution) |
| `native` | An OS binary speaking the same Workers RPC ABI. The host starts two sibling jails, `bookclerk-workerd` (control plane) and the native backend, joined by inherited duplex links |

TypeScript and Python plugins are workerd plugins. Rust plugins are
native, or workerd when compiled to Wasm with the SDK's `workerd` glue.

## What every SDK gives you

- The entrypoint base classes: `BookclerkEntrypoint` for the default
  export, and one `*Entrypoint` base per named entrypoint
  (`Storefront`, `Storage`, `RemoteLibrary`, `DatabaseAdapter`, `Cli`,
  `Oidc`, `Metadata`, `Covers`).
- The ABI types, generated from
  [`plugin.capnp`](../../../crates/bookclerk-plugin-abi/schema/plugin.capnp)
  so all three SDKs speak the same wire.
- An author CLI: `check` validates the package against the author model,
  `types` writes an `Env` type from `plugin.toml`, `fmt` formats the
  manifest, `package` builds the archive, and `smoke` runs a workerd
  plugin against the pinned `workerd` without a built host.

## Next steps

- [plugin.toml](../manifest.md): the manifest every plugin starts with.
- [Entrypoints, events and the ABI](../abi.md): the methods each
  entrypoint implements.
- [Dynamic plugins](../README.md#develop-locally): run a plugin against a
  local host.
