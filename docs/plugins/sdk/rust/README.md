# Rust plugin SDK

This page shows how to write a Bookclerk plugin in Rust with
`bookclerk-plugin-sdk` (`api_version = 3`). A Rust plugin is a native
guest, a binary the host runs in its own jail, or a workerd guest when
compiled to Wasm with the SDK's `workerd` glue. Depend on this crate
alone, never on `bookclerk-plugin-host`, which pulls in the host's
internals.

## Install

```toml
[dependencies]
bookclerk-plugin-sdk = { git = "https://github.com/Bookclerk/bookclerk", package = "bookclerk-plugin-sdk" }
# Database guests also need:
# bookclerk-plugin-sdk = { …, features = ["db"] }
```

| Feature | Purpose |
| --- | --- |
| *(default)* | The Cap'n Proto guest runner, fetch and upload path helpers, the callback tunnel, and the ABI re-exports |
| `db` | SeaORM to Workers RPC DTO helpers for database guests |
| `http` | Native HTTPS through the workerd socket proxy |

## Author model

A native guest implements `PluginWorker` (`describe`, and
`open(invocation, bindings) -> Entrypoints`) and calls `serve`, the
stdin and stdout Cap'n Proto runner. The entrypoints it returns are the
same named entrypoints the other SDKs export as classes: storefront,
storage, remote library, database adapter, CLI, OIDC, metadata and
covers. The
crate root re-exports the DTOs and helpers a guest needs; prefer
`use bookclerk_plugin_sdk::…` over reaching into submodules.

Identity (`apiVersion`, `id`, capabilities) comes from `plugin.toml`;
`describe` only refines `displayName` and similar fields.

A `[[databases]]` binding is taken from the `open` bindings with
`DatabaseBinding::take_named_from_bindings` and used as on D1
(`prepare`, `bind`, `all`, `first`, `batch`). Its `limits()` returns the
session's `DbCapabilities`; `all()` reads through the session's streamed
budget when it advertises one, and `chunk()` splits a long statement list
into batches that each fit one request. Any cap that is hit is a
`PluginError` with `PluginErrorCode::LimitExceeded` and a typed `limit()`;
see [Working within database limits](../../database-limits.md).

## The socket proxy

A native guest has no ambient network. It receives an inherited link
(`fd:<n>` on Unix, `handle:<n>` on Windows) in `BOOKCLERK_SOCKET_PROXY`
and multiplexes CONNECT streams over it with the `mux` module; `net`
opens mediated TCP sockets, the equivalent of Workers `connect()`, and
the `http` feature puts an HTTPS client on top. Only hosts the plugin's
`[capabilities.network]` grants name are reachable.

Older SDKs that only opened a pathname or named-pipe proxy cannot reach
the host and fail closed; rebuild guests against this SDK. Pathname,
`abstract:` and `\\.\pipe\` forms remain for tests.

## Author tools

The `bookclerk-plugin` author CLI (`check`, `fmt`, `sync-embed`,
`package`, `smoke` and `test`) lives in the separate `bookclerk-plugin-tools`
crate, so a guest that depends on this SDK never links
`bookclerk-workerd`. Install it once, then run it from the plugin's
directory:

```bash
cargo install --git https://github.com/Bookclerk/bookclerk bookclerk-plugin-tools
bookclerk-plugin check .
bookclerk-plugin smoke .
bookclerk-plugin test .    # the plugin's suite under every database profile
```

Inside a Bookclerk checkout, `cargo plugin -- check <dir>` is an alias
for the same binary (`.cargo/config.toml`).

## Next steps

- [plugin.toml](../../manifest.md): declare the entrypoints the guest
  serves.
- [Entrypoints, events and the ABI](../../abi.md): every method and
  event in detail.
- [`bookclerk_plugin_abi`](../../../internals/crates/bookclerk_plugin_abi/README.md)
  and [`bookclerk_plugin_manifest`](../../../internals/crates/bookclerk_plugin_manifest/README.md):
  the ABI and manifest crates the SDK re-exports.
- [Try the Echo example](../../README.md#try-the-echo-example): complete
  native and workerd Rust plugins under `examples/`.

## Reference

The sections below are generated from the crate's source: the crate
root, with its items and re-exports, and one page per public module.
