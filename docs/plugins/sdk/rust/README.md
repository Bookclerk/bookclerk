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
storage, remote library, database adapter, CLI, OIDC, metadata, covers,
metadata files, notifier and model plugin (`ai`). The
crate root re-exports the DTOs and helpers a guest needs; prefer
`use bookclerk_plugin_sdk::…` over reaching into submodules.

Identity (`apiVersion`, `id`, capabilities) comes from `plugin.toml`;
`describe` only refines `displayName` and similar fields.

A `[[databases]]` binding is taken from the `open` bindings with
`DatabaseBinding::take_named_from_bindings` and used as on D1
(`prepare`, `bind`, `all`, `first`, `batch`). Its `limits()` returns the
session's `DbCapabilities`; `all()` reads through the session's streamed
budget when it advertises one, `iterate()` walks the same read one row at
a time without holding the whole result, `chunk()` splits a long statement list
into batches that each fit one request, and `write_chunks()` commits those
batches resumably. Any cap that is hit is a
`PluginError` with `PluginErrorCode::LimitExceeded` and a typed `limit()`;
see [Working within database limits](../../database-limits.md). A guest's
tests open the same binding with `bookclerk_db_testkit::BindingDatabase`
under `bookclerk_db_testkit::selected_profile()`, so `bookclerk-plugin test .`
runs them under every profile (see
[Testing against the limits](../../database-limits.md#testing-against-the-limits)).
A storefront's `scan` can hand its paging to `scan_page(&params, fetch)`,
which slices the store's own pages into replies the host accepts; see
[Paged scans](../../abi.md#paged-scans).
[`examples/plugins-echo-native-rust`](../../../../examples/plugins-echo-native-rust)
does all of this end to end in its `notes` table.

## The socket proxy

A native guest has no ambient network. It receives an inherited link
(`fd:<n>` on Unix, `handle:<n>` on Windows) in `BOOKCLERK_SOCKET_PROXY`
and multiplexes CONNECT streams over it with the `mux` module; `net`
opens mediated TCP sockets, the equivalent of Workers `connect()`, and
the `http` feature puts an HTTPS client on top. Only hosts the plugin's
`[capabilities.network]` grants name are reachable.

A protocol that starts in the clear and upgrades (SMTP's `STARTTLS`) or
speaks TLS without HTTP takes the socket with `Socket::into_stream` and
calls `http::start_tls(hostname, stream)`, which checks the server's
certificate against the Mozilla roots. A destination the operator types
into a `network = "tcp"` setting arrives in `Bindings::network`
(`NetworkGrant`, with a `SettingGrantState` of approved, pending, unset or
invalid for each setting); the first-party email notifier reads its mail
server that way.

### Large downloads

A storefront that writes a title into the `fetchTitle` work folder should
download it with `bookclerk_plugin_sdk::download::Download` (feature
`http`) rather than reading the body into memory. It streams the body to
`<dest>.part` and, when the connection drops, stalls (no data for the idle
timeout, 60 seconds by default) or the CDN answers `408`, `429` or `5xx`,
asks again for only the missing bytes with `Range`, and `If-Range` when
the CDN sent an `ETag` or `Last-Modified`. A CDN that ignores the range
gets the whole file again; an answer for another range or a changed
length starts over, so bytes are never spliced from two versions.

```rust
use bookclerk_plugin_sdk::download::Download;

let done = Download::new()
    .expected_size(store_size) // when the store states it
    .to_file(&work_dir.join("book.m4b"), || http.get(&url))
    .await?;
```

The closure builds a fresh request for each attempt (URL, auth, cookies);
the download adds the range headers. `DownloadError::is_transient` tells a
failure worth another `fetchTitle` from a refusal. The decisions
themselves are the sans-IO functions in `bookclerk_plugin_sdk::resume`,
which a guest with its own HTTP stack can reuse.

> [!NOTE]
> The host clears the work folder after every `fetchTitle`, so a download
> resumes within one call. A partial kept across failed attempts or a
> host restart is not supported yet.

Older SDKs that only opened a pathname or named-pipe proxy cannot reach
the host and fail closed; rebuild guests against this SDK. Pathname,
`abstract:` and `\\.\pipe\` forms remain for tests.

## Pushed sockets

A plugin that declares `[sockets]` (see
[Pushed sockets](../../abi.md#pushed-sockets-sockets)) receives `Sockets`
on `Bindings::sockets` in `open` and may return a `SocketHandler` on
`Entrypoints::socket_handler`. The host holds each connection, answers the
declared heartbeats and calls the handler once per message, so the plugin
process may sleep in between. The `sockets` module wraps one socket id
in `HostWebSocket` (`send_text`, `close`, `serialize_attachment`,
`deserialize_attachment`), the same surface the TypeScript and Python
SDKs give their handlers, and `text_heartbeat("2", "3")` builds a
heartbeat pair. Attachments are JSON of at most 2 KiB, so a socket reads
the same from every SDK. `SocketHandler::message` returns a `SocketMessageOk`; a remote
library sets `poll_events` when a message announced a new user, and the
host calls `poll_events` right away instead of at its next scheduled
poll. The Audiobookshelf plugin, written in TypeScript, is a worked
example of the same conversation
(`crates/bookclerk-plugins/optional/integration-audiobookshelf/src/push.ts`).

## Inbound webhooks

A plugin that declares `[[webhooks]]` (see
[Inbound webhooks](../../abi.md#inbound-webhooks-webhooks)) receives
`Webhooks` on `Bindings::webhooks` in `open` (`url`, `pull` and `ack`,
with `WebhookPullRequest` and `WebhookAckRequest`), and may return a
`WebhookHandler` on `Entrypoints::webhook_handler`, whose `delivered` the
host calls on a running guest once a request is stored. The SDK re-exports
these types and the `MAX_WEBHOOK_PULL_BATCH` and
`MAX_WEBHOOK_VISIBILITY_MS` limits. `bookclerk_plugin_abi::webhooks` has the checks the host applies, so a
plugin can validate a pull before sending it.

## Author tools

The `bookclerk-plugin` author CLI (`check`, `fmt`, `sync-embed`,
`package`, `smoke` and `test`) lives in the separate `bookclerk-plugin-tools`
crate, so a guest that depends on this SDK never links
`bookclerk-workerd`. Install it once, then run it from the plugin's
directory:

```bash
cargo install --git https://github.com/Bookclerk/bookclerk bookclerk-plugin-tools
cargo install --git https://github.com/Bookclerk/bookclerk bookclerk-db-testkit  # the kit `test` serves
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
